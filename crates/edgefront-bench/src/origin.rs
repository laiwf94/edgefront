//! The benchmark origin: the server behind every target.
//!
//! It answers `GET /obj/<size>/<name>` (the paths [`crate::scenario`]
//! asks for) with `size` bytes and the same cacheable headers for every
//! target, so the targets differ only in what they do with them:
//!
//! - `Cache-Control: public, max-age=3600`, long enough that nothing
//!   expires during a run, so every target's hit ratio is set by the
//!   scenario and its cache size, never by its TTL handling;
//! - a strong `ETag` and a fixed `Last-Modified`, for targets that
//!   revalidate;
//! - `Content-Length`, with the body streamed in 1 MiB frames cut from
//!   one shared buffer, so a 1 GiB object costs the origin no memory.
//!
//! It counts the object requests and body bytes it serves; a load run
//! reads `GET /__origin/stats` before and after the measured phase to see
//! how many requests a target sent upstream (its misses). `HEAD` is
//! answered like `GET` without a body. `GET /__origin/healthz` answers
//! `ok`. Anything else is 404.
//!
//! **HTTP/2.** Every connection may speak HTTP/1.1 or HTTP/2 with prior
//! knowledge (h2c): one that opens with the HTTP/2 preface is served as
//! HTTP/2, with room for 10,000 concurrent streams, so a proxy can
//! multiplex as many as it likes upstream.
//!
//! **gRPC.** A unary call to [`GRPC_METHOD`] (`POST`, `content-type:
//! application/grpc`) whose one message is an object path is answered
//! with one message of that object's bytes and `grpc-status: 0` in the
//! trailers, and counted like a `GET` of the object. A call with no
//! valid path gets a Trailers-Only `INVALID_ARGUMENT`, one for a path that
//! is not an object `NOT_FOUND`, and one for another method
//! `UNIMPLEMENTED`.
//!
//! **Delay.** [`Origin::bind_with_delay`] (`edgefront-bench origin
//! --delay`) holds every object request, `GET` or gRPC, that long before
//! answering, so a scenario's `origin_delay` sets how many streams stay
//! open through the target.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::server::conn::{http1, http2};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;

/// The gRPC method the origin serves: one object a call.
pub const GRPC_METHOD: &str = "/edgefront.bench.Objects/Get";

/// The longest request message the origin reads, an object path with room
/// to spare.
const MAX_GRPC_REQUEST: usize = 4096;

/// The HTTP/2 connection preface's first bytes: no HTTP/1.1 method starts
/// with them.
const PREFACE_START: &[u8] = b"PRI ";

/// How long a new connection may take to send its first bytes.
const FIRST_BYTES_WITHIN: Duration = Duration::from_secs(10);

/// The concurrent streams an HTTP/2 connection to the origin may open.
pub const MAX_STREAMS: u32 = 10_000;

/// `message` as one uncompressed gRPC length-prefixed message.
pub fn grpc_frame(message: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(5 + message.len());
    out.put_u8(0);
    out.put_u32(message.len() as u32);
    out.put_slice(message);
    out.freeze()
}

/// The unary call for the object at `path`, as the load generator sends
/// it: `POST` [`GRPC_METHOD`] with the path as its one message.
pub fn grpc_request(path: &str) -> Request<Full<Bytes>> {
    let mut request = Request::new(Full::new(grpc_frame(path.as_bytes())));
    *request.method_mut() = Method::POST;
    *request.uri_mut() = http::Uri::from_static(GRPC_METHOD);
    let headers = request.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc"),
    );
    headers.insert(header::TE, HeaderValue::from_static("trailers"));
    request
}

/// Whether a request is a gRPC call, by its `content-type`.
fn is_grpc<B>(request: &Request<B>) -> bool {
    request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v == "application/grpc"
                || v.starts_with("application/grpc+")
                || v.starts_with("application/grpc;")
        })
}

/// The one message of a unary call's body, uncompressed and whole.
fn grpc_message(body: &[u8]) -> Option<&[u8]> {
    let (&flag, rest) = body.split_first()?;
    if flag != 0 || rest.len() < 4 {
        return None;
    }
    let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
    let message = &rest[4..];
    (message.len() == len).then_some(message)
}

/// The largest object the origin serves, as [`crate::scenario::MAX_OBJECT`].
pub const MAX_SIZE: u64 = crate::scenario::MAX_OBJECT;

/// The frame size bodies are streamed in.
const FRAME: usize = 1 << 20;

/// The fixed `Last-Modified` of every object.
const LAST_MODIFIED: &str = "Sun, 01 Nov 2026 00:00:00 GMT";

/// The `Cache-Control` of every object.
pub const CACHE_CONTROL: &str = "public, max-age=3600";

/// What the origin has served since it started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginStats {
    /// Object requests answered, `GET` and `HEAD`.
    pub requests: u64,
    /// Object body bytes sent.
    pub bytes: u64,
}

impl OriginStats {
    /// The requests and bytes served between `earlier` and `self`.
    pub fn since(&self, earlier: &OriginStats) -> OriginStats {
        OriginStats {
            requests: self.requests.saturating_sub(earlier.requests),
            bytes: self.bytes.saturating_sub(earlier.bytes),
        }
    }
}

#[derive(Default)]
struct Counters {
    requests: AtomicU64,
    bytes: AtomicU64,
}

/// A running origin.
pub struct Origin {
    addr: SocketAddr,
    counters: Arc<Counters>,
    task: tokio::task::JoinHandle<()>,
}

impl Origin {
    /// Binds `addr` and serves until dropped.
    pub async fn bind(addr: SocketAddr) -> std::io::Result<Origin> {
        Self::bind_with_delay(addr, Duration::ZERO).await
    }

    /// As [`Origin::bind`], answering each object request after `delay`:
    /// a slow origin, for the harness's own tests.
    pub async fn bind_with_delay(addr: SocketAddr, delay: Duration) -> std::io::Result<Origin> {
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let counters = Arc::new(Counters::default());
        let task = tokio::spawn(serve(listener, Arc::clone(&counters), delay));
        Ok(Origin {
            addr,
            counters,
            task,
        })
    }

    /// The bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// What it has served so far.
    pub fn stats(&self) -> OriginStats {
        stats(&self.counters)
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn stats(counters: &Counters) -> OriginStats {
    OriginStats {
        requests: counters.requests.load(Ordering::Relaxed),
        bytes: counters.bytes.load(Ordering::Relaxed),
    }
}

async fn serve(listener: TcpListener, counters: Arc<Counters>, delay: Duration) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            // Out of descriptors or similar: back off instead of spinning.
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };
        let _ = stream.set_nodelay(true);
        let counters = Arc::clone(&counters);
        tokio::spawn(async move {
            let mut stream = stream;
            let Ok(Some((h2, first))) =
                tokio::time::timeout(FIRST_BYTES_WITHIN, opens_with_preface(&mut stream)).await
            else {
                return;
            };
            let stream = Prefixed {
                prefix: first,
                inner: stream,
            };
            let service = service_fn(move |request: Request<Incoming>| {
                let counters = Arc::clone(&counters);
                async move { Ok::<_, Infallible>(route(&counters, request, delay).await) }
            });
            let io = TokioIo::new(stream);
            if h2 {
                let _ = http2::Builder::new(TokioExecutor::new())
                    .max_concurrent_streams(MAX_STREAMS)
                    .serve_connection(io, service)
                    .await;
            } else {
                let _ = http1::Builder::new().serve_connection(io, service).await;
            }
        });
    }
}

/// Whether the connection opens with the HTTP/2 preface, and the bytes
/// read to tell: up to the first that differs from it, or its first four.
/// `None` when the connection closes or fails first.
async fn opens_with_preface<S: AsyncRead + Unpin>(stream: &mut S) -> Option<(bool, Bytes)> {
    let mut buf = [0u8; PREFACE_START.len()];
    let mut len = 0;
    while len < buf.len() {
        let n = stream.read(&mut buf[len..]).await.ok()?;
        if n == 0 {
            return None;
        }
        len += n;
        if buf[..len] != PREFACE_START[..len] {
            return Some((false, Bytes::copy_from_slice(&buf[..len])));
        }
    }
    Some((true, Bytes::copy_from_slice(&buf)))
}

/// A stream whose first bytes were already read: they are read again
/// first.
struct Prefixed<S> {
    prefix: Bytes,
    inner: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.prefix.is_empty() {
            let n = self.prefix.len().min(buf.remaining());
            let head = self.prefix.split_to(n);
            buf.put_slice(&head);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, data)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// gRPC calls to [`answer_grpc`], everything else to [`answer`].
async fn route(
    counters: &Counters,
    request: Request<Incoming>,
    delay: Duration,
) -> Response<ObjectBody> {
    if is_grpc(&request) {
        let (parts, body) = request.into_parts();
        let body = Limited::new(body, MAX_GRPC_REQUEST + 5)
            .collect()
            .await
            .map(|b| b.to_bytes())
            .ok();
        return answer_grpc(
            counters,
            &parts.method,
            parts.uri.path(),
            body.as_deref(),
            delay,
        )
        .await;
    }
    answer(counters, &request, delay).await
}

/// gRPC status codes the origin answers with.
const GRPC_OK: u32 = 0;
const GRPC_INVALID_ARGUMENT: u32 = 3;
const GRPC_NOT_FOUND: u32 = 5;
const GRPC_UNIMPLEMENTED: u32 = 12;

/// A Trailers-Only gRPC answer: HTTP 200 whose headers carry the status.
fn grpc_status_only(code: u32, message: &'static str) -> Response<ObjectBody> {
    let mut response = Response::new(ObjectBody::empty());
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc"),
    );
    headers.insert("grpc-status", HeaderValue::from(code));
    headers.insert("grpc-message", HeaderValue::from_static(message));
    response
}

/// The answer to a gRPC call: `body` is the request's whole body, `None`
/// when it could not be read or was too long.
async fn answer_grpc(
    counters: &Counters,
    method: &Method,
    path: &str,
    body: Option<&[u8]>,
    delay: Duration,
) -> Response<ObjectBody> {
    if method != Method::POST || path != GRPC_METHOD {
        return grpc_status_only(GRPC_UNIMPLEMENTED, "no such method");
    }
    let Some(object) = body
        .and_then(grpc_message)
        .and_then(|m| std::str::from_utf8(m).ok())
    else {
        return grpc_status_only(GRPC_INVALID_ARGUMENT, "one object path expected");
    };
    let Some(size) = object_size(object) else {
        return grpc_status_only(GRPC_NOT_FOUND, "not an object");
    };
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    counters.requests.fetch_add(1, Ordering::Relaxed);
    counters.bytes.fetch_add(size, Ordering::Relaxed);
    let mut prefix = BytesMut::with_capacity(5);
    prefix.put_u8(0);
    prefix.put_u32(size as u32);
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", HeaderValue::from(GRPC_OK));
    let mut response = Response::new(ObjectBody {
        fixed: Some(prefix.freeze()),
        remaining: size,
        trailers: Some(trailers),
    });
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc"),
    );
    response
}

/// Parses `/obj/<size>/<name>` into the size, for a size up to
/// [`MAX_SIZE`] and a non-empty name of letters, digits, `-`, `_` and `.`.
pub fn object_size(path: &str) -> Option<u64> {
    let rest = path.strip_prefix("/obj/")?;
    let (size, name) = rest.split_once('/')?;
    if size.is_empty() || size.len() > 12 || !size.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return None;
    }
    let size: u64 = size.parse().ok()?;
    (size <= MAX_SIZE).then_some(size)
}

async fn answer<B>(
    counters: &Counters,
    request: &Request<B>,
    delay: Duration,
) -> Response<ObjectBody> {
    let path = request.uri().path();
    let head = request.method() == Method::HEAD;
    if request.method() != Method::GET && !head {
        return plain(StatusCode::METHOD_NOT_ALLOWED, "");
    }
    match path {
        "/__origin/healthz" => return plain(StatusCode::OK, "ok\n"),
        "/__origin/stats" => {
            let body = serde_json::to_string(&stats(counters)).unwrap_or_default();
            let mut response = plain(StatusCode::OK, "");
            *response.body_mut() = ObjectBody::fixed(Bytes::from(body));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            return response;
        }
        _ => {}
    }
    let Some(size) = object_size(path) else {
        return plain(StatusCode::NOT_FOUND, "not found\n");
    };
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    counters.requests.fetch_add(1, Ordering::Relaxed);
    if !head {
        counters.bytes.fetch_add(size, Ordering::Relaxed);
    }
    let name = &path[path.rfind('/').map_or(0, |i| i + 1)..];
    let body = if head {
        ObjectBody::empty()
    } else {
        ObjectBody::pattern(size)
    };
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        header::LAST_MODIFIED,
        HeaderValue::from_static(LAST_MODIFIED),
    );
    // The name and size are checked ASCII, so the value is valid.
    if let Ok(etag) = HeaderValue::from_str(&format!("\"{size}-{name}\"")) {
        headers.insert(header::ETAG, etag);
    }
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    response
}

fn plain(status: StatusCode, text: &'static str) -> Response<ObjectBody> {
    let mut response = Response::new(ObjectBody::fixed(Bytes::from_static(text.as_bytes())));
    *response.status_mut() = status;
    response
}

/// One shared buffer of the byte pattern every body repeats.
fn pattern() -> &'static Bytes {
    static PATTERN: OnceLock<Bytes> = OnceLock::new();
    PATTERN.get_or_init(|| Bytes::from((0..FRAME).map(|i| (i % 251) as u8).collect::<Vec<u8>>()))
}

/// A body of known length: a fixed buffer, then `remaining` bytes of the
/// pattern in frames of at most 1 MiB, then trailers if it has any.
pub struct ObjectBody {
    fixed: Option<Bytes>,
    remaining: u64,
    trailers: Option<HeaderMap>,
}

impl ObjectBody {
    fn fixed(bytes: Bytes) -> Self {
        ObjectBody {
            fixed: Some(bytes),
            remaining: 0,
            trailers: None,
        }
    }

    fn empty() -> Self {
        ObjectBody {
            fixed: None,
            remaining: 0,
            trailers: None,
        }
    }

    fn pattern(size: u64) -> Self {
        ObjectBody {
            fixed: None,
            remaining: size,
            trailers: None,
        }
    }
}

impl Body for ObjectBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if let Some(bytes) = self.fixed.take()
            && !bytes.is_empty()
        {
            return Poll::Ready(Some(Ok(Frame::data(bytes))));
        }
        if self.remaining == 0 {
            return Poll::Ready(self.trailers.take().map(|t| Ok(Frame::trailers(t))));
        }
        let take = self.remaining.min(FRAME as u64) as usize;
        self.remaining -= take as u64;
        Poll::Ready(Some(Ok(Frame::data(pattern().slice(..take)))))
    }

    fn is_end_stream(&self) -> bool {
        self.fixed.as_ref().is_none_or(Bytes::is_empty)
            && self.remaining == 0
            && self.trailers.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        let fixed = self.fixed.as_ref().map_or(0, |b| b.len() as u64);
        SizeHint::with_exact(fixed + self.remaining)
    }
}

/// Reads an origin's counters over HTTP: `GET /__origin/stats` on `addr`.
pub async fn fetch_stats(addr: SocketAddr) -> Result<OriginStats, String> {
    let bytes = crate::client::get(addr, "/__origin/stats", Duration::from_secs(5))
        .await
        .map_err(|e| format!("origin stats from {addr}: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("origin stats from {addr}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpStream;

    #[test]
    fn parses_object_paths() {
        assert_eq!(object_size("/obj/4096/k1"), Some(4096));
        assert_eq!(object_size("/obj/0/u7"), Some(0));
        assert_eq!(object_size("/obj/1073741824/a-b_c.d"), Some(1 << 30));
        for bad in [
            "/obj/1073741825/k1",
            "/obj//k1",
            "/obj/12/",
            "/obj/12",
            "/obj/x/k1",
            "/obj/-1/k1",
            "/obj/1/k/2",
            "/obj/1/k%20",
            "/obj/9999999999999/k1",
            "/other/1/k1",
        ] {
            assert_eq!(object_size(bad), None, "{bad}");
        }
    }

    #[test]
    fn stats_subtract() {
        let a = OriginStats {
            requests: 10,
            bytes: 100,
        };
        let b = OriginStats {
            requests: 4,
            bytes: 40,
        };
        assert_eq!(
            a.since(&b),
            OriginStats {
                requests: 6,
                bytes: 60
            }
        );
        assert_eq!(b.since(&a), OriginStats::default());
    }

    async fn collect(response: Response<ObjectBody>) -> (Response<()>, Bytes) {
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        (Response::from_parts(parts, ()), bytes)
    }

    fn get(method: Method, path: &str) -> Request<()> {
        Request::builder()
            .method(method)
            .uri(path)
            .body(())
            .unwrap()
    }

    #[tokio::test]
    async fn serves_objects_of_the_named_size() {
        let counters = Counters::default();
        let size = (FRAME as u64) * 2 + 17;
        let response = answer(
            &counters,
            &get(Method::GET, &format!("/obj/{size}/k3")),
            Duration::ZERO,
        )
        .await;
        assert_eq!(response.body().size_hint().exact(), Some(size));
        let (head, body) = collect(response).await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers()[header::CACHE_CONTROL], CACHE_CONTROL);
        assert_eq!(head.headers()[header::ETAG], format!("\"{size}-k3\""));
        assert_eq!(head.headers()[header::LAST_MODIFIED], LAST_MODIFIED);
        assert_eq!(head.headers()[header::CONTENT_LENGTH], size.to_string());
        assert_eq!(body.len() as u64, size);
        assert_eq!(body[FRAME + 1], 1);
        assert_eq!(body[250], 250);
        assert_eq!(body[251], 0);

        let (head, body) =
            collect(answer(&counters, &get(Method::HEAD, "/obj/10/k3"), Duration::ZERO).await)
                .await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers()[header::CONTENT_LENGTH], "10");
        assert!(body.is_empty());
        assert_eq!(
            stats(&counters),
            OriginStats {
                requests: 2,
                bytes: size
            }
        );

        let (head, body) =
            collect(answer(&counters, &get(Method::GET, "/obj/0/k0"), Duration::ZERO).await).await;
        assert_eq!(head.status(), StatusCode::OK);
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn refuses_other_paths_and_methods() {
        let counters = Counters::default();
        let (head, _) =
            collect(answer(&counters, &get(Method::GET, "/nope"), Duration::ZERO).await).await;
        assert_eq!(head.status(), StatusCode::NOT_FOUND);
        let (head, _) =
            collect(answer(&counters, &get(Method::POST, "/obj/1/k1"), Duration::ZERO).await).await;
        assert_eq!(head.status(), StatusCode::METHOD_NOT_ALLOWED);
        let (head, body) = collect(
            answer(
                &counters,
                &get(Method::GET, "/__origin/healthz"),
                Duration::ZERO,
            )
            .await,
        )
        .await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(&body[..], b"ok\n");
        assert_eq!(stats(&counters), OriginStats::default());
    }

    #[tokio::test]
    async fn serves_and_reports_over_tcp() {
        let origin = Origin::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let body = crate::client::get(origin.addr(), "/obj/5000/k1", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(body.len(), 5000);
        assert_eq!(
            fetch_stats(origin.addr()).await.unwrap(),
            OriginStats {
                requests: 1,
                bytes: 5000
            }
        );
        assert_eq!(origin.stats().requests, 1);
    }

    #[tokio::test]
    async fn a_delayed_origin_waits_before_answering() {
        let origin =
            Origin::bind_with_delay("127.0.0.1:0".parse().unwrap(), Duration::from_millis(50))
                .await
                .unwrap();
        let start = std::time::Instant::now();
        crate::client::get(origin.addr(), "/obj/1/k1", Duration::from_secs(5))
            .await
            .unwrap();
        assert!(start.elapsed() >= Duration::from_millis(50));
    }

    #[tokio::test]
    async fn stats_from_a_server_that_is_not_an_origin_fail() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        assert!(
            fetch_stats(addr)
                .await
                .unwrap_err()
                .contains("origin stats")
        );
        let origin = Origin::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let err = crate::client::get(origin.addr(), "/nope", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.contains("404"), "{err}");
    }

    /// One HTTP/2 prior-knowledge connection to `addr`.
    async fn h2(addr: SocketAddr) -> hyper::client::conn::http2::SendRequest<Full<Bytes>> {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (sender, connection) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
                .await
                .unwrap();
        tokio::spawn(connection);
        sender
    }

    fn absolute(mut request: Request<Full<Bytes>>, addr: SocketAddr) -> Request<Full<Bytes>> {
        let path = request.uri().path().to_owned();
        *request.uri_mut() = format!("http://{addr}{path}").parse().unwrap();
        request
    }

    /// A response's status, `grpc-status` (trailers, else headers), and
    /// body.
    async fn call(
        sender: &mut hyper::client::conn::http2::SendRequest<Full<Bytes>>,
        request: Request<Full<Bytes>>,
    ) -> (StatusCode, Option<String>, Bytes) {
        let response = sender.send_request(request).await.unwrap();
        let status = response.status();
        let header = response
            .headers()
            .get("grpc-status")
            .map(|v| v.to_str().unwrap().to_owned());
        let collected = response.into_body().collect().await.unwrap();
        let trailer = collected
            .trailers()
            .and_then(|t| t.get("grpc-status"))
            .map(|v| v.to_str().unwrap().to_owned());
        (status, trailer.or(header), collected.to_bytes())
    }

    #[test]
    fn frames_and_reads_grpc_messages() {
        let frame = grpc_frame(b"/obj/10/k1");
        assert_eq!(&frame[..5], &[0, 0, 0, 0, 10]);
        assert_eq!(grpc_message(&frame), Some(&b"/obj/10/k1"[..]));
        assert_eq!(grpc_message(&grpc_frame(b"")), Some(&b""[..]));
        // Compressed, short, truncated or followed by a second message.
        let mut compressed = frame.to_vec();
        compressed[0] = 1;
        assert_eq!(grpc_message(&compressed), None);
        assert_eq!(grpc_message(&frame[..4]), None);
        assert_eq!(grpc_message(&frame[..9]), None);
        assert_eq!(grpc_message(&[&frame[..], &frame[..]].concat()), None);
        assert_eq!(grpc_message(&[]), None);

        let request = grpc_request("/obj/10/k1");
        assert_eq!(request.method(), Method::POST);
        assert_eq!(request.uri().path(), GRPC_METHOD);
        assert_eq!(request.headers()[header::TE], "trailers");
        assert!(is_grpc(&request));
        for (value, grpc) in [
            ("application/grpc", true),
            ("application/grpc+proto", true),
            ("application/grpc;charset=utf-8", true),
            ("application/grpc-web", false),
            ("application/json", false),
        ] {
            let r = Request::builder()
                .header(header::CONTENT_TYPE, value)
                .body(())
                .unwrap();
            assert_eq!(is_grpc(&r), grpc, "{value}");
        }
        assert!(!is_grpc(&Request::new(())));
    }

    #[tokio::test]
    async fn serves_http2_and_http1_on_one_port() {
        let origin = Origin::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = origin.addr();
        // HTTP/1.1 still works beside it.
        let body = crate::client::get(addr, "/obj/300/k1", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(body.len(), 300);

        let mut sender = h2(addr).await;
        let get = |path: &str| {
            Request::builder()
                .uri(format!("http://{addr}{path}"))
                .body(Full::new(Bytes::new()))
                .unwrap()
        };
        let (status, _, body) = call(&mut sender, get("/obj/70000/k2")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.len(), 70_000);
        // Many streams at once on the one connection.
        let mut calls = Vec::new();
        for i in 0..300 {
            let mut sender = sender.clone();
            let request = get(&format!("/obj/10/k{i}"));
            calls.push(tokio::spawn(
                async move { call(&mut sender, request).await },
            ));
        }
        for c in calls {
            assert_eq!(c.await.unwrap().2.len(), 10);
        }
        assert_eq!(origin.stats().requests, 302);
    }

    #[tokio::test]
    async fn answers_unary_grpc_calls_with_the_object() {
        let origin = Origin::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = origin.addr();
        let mut sender = h2(addr).await;

        let (status, grpc, body) =
            call(&mut sender, absolute(grpc_request("/obj/5000/k7"), addr)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(grpc.as_deref(), Some("0"));
        assert_eq!(body.len(), 5005);
        assert_eq!(&body[..5], &[0, 0, 0, 0x13, 0x88]);
        assert_eq!(body[5 + 250], 250);
        assert_eq!(
            origin.stats(),
            OriginStats {
                requests: 1,
                bytes: 5000
            }
        );

        // An empty object is one empty message.
        let (_, grpc, body) = call(&mut sender, absolute(grpc_request("/obj/0/k0"), addr)).await;
        assert_eq!(
            (grpc.as_deref(), &body[..]),
            (Some("0"), &[0u8, 0, 0, 0, 0][..])
        );

        // Trailers-Only errors, none of them counted.
        let (status, grpc, body) = call(&mut sender, absolute(grpc_request("/nope/1"), addr)).await;
        assert_eq!((status, grpc.as_deref()), (StatusCode::OK, Some("5")));
        assert!(body.is_empty());
        let mut other = absolute(grpc_request("/obj/1/k1"), addr);
        *other.uri_mut() = format!("http://{addr}/edgefront.bench.Objects/Put")
            .parse()
            .unwrap();
        assert_eq!(call(&mut sender, other).await.1.as_deref(), Some("12"));
        let mut garbled = absolute(grpc_request("/obj/1/k1"), addr);
        *garbled.body_mut() = Full::new(Bytes::from_static(b"\x01\x00\x00\x00\x01x"));
        assert_eq!(call(&mut sender, garbled).await.1.as_deref(), Some("3"));
        let mut huge = absolute(grpc_request("/obj/1/k1"), addr);
        *huge.body_mut() = Full::new(grpc_frame(&vec![b'a'; MAX_GRPC_REQUEST + 1]));
        assert_eq!(call(&mut sender, huge).await.1.as_deref(), Some("3"));
        assert_eq!(origin.stats().requests, 2);
    }

    #[tokio::test]
    async fn the_delay_holds_grpc_calls_and_streams_stay_open_together() {
        let origin =
            Origin::bind_with_delay("127.0.0.1:0".parse().unwrap(), Duration::from_millis(200))
                .await
                .unwrap();
        let addr = origin.addr();
        let sender = h2(addr).await;
        let start = std::time::Instant::now();
        let mut calls = Vec::new();
        for i in 0..500 {
            let mut sender = sender.clone();
            let request = absolute(grpc_request(&format!("/obj/1/k{i}")), addr);
            calls.push(tokio::spawn(
                async move { call(&mut sender, request).await },
            ));
        }
        for c in calls {
            assert_eq!(c.await.unwrap().1.as_deref(), Some("0"));
        }
        // 500 streams held 200 ms each finish together, not one by one.
        let took = start.elapsed();
        assert!(
            took >= Duration::from_millis(200) && took < Duration::from_secs(5),
            "{took:?}"
        );
    }

    #[tokio::test]
    async fn the_first_bytes_choose_the_protocol_and_are_read_again() {
        use tokio::io::AsyncWriteExt;
        async fn decide(parts: &[&[u8]]) -> (Option<(bool, Bytes)>, Vec<u8>) {
            let (mut client, mut server) = tokio::io::duplex(64);
            for part in parts {
                client.write_all(part).await.unwrap();
            }
            drop(client);
            let decision = opens_with_preface(&mut server).await;
            let prefix = decision.as_ref().map(|d| d.1.clone()).unwrap_or_default();
            let mut all = Vec::new();
            Prefixed {
                prefix,
                inner: server,
            }
            .read_to_end(&mut all)
            .await
            .unwrap();
            (decision.map(|(h2, _)| (h2, Bytes::new())), all)
        }
        let (d, all) = decide(&[b"PR", b"I * HTTP/2.0\r\n"]).await;
        assert_eq!(d.map(|d| d.0), Some(true));
        assert_eq!(all, b"PRI * HTTP/2.0\r\n");
        let (d, all) = decide(&[b"GET / HTTP/1.1\r\n"]).await;
        assert_eq!(d.map(|d| d.0), Some(false));
        assert_eq!(all, b"GET / HTTP/1.1\r\n");
        let (d, all) = decide(&[b"P", b"OST /"]).await;
        assert_eq!(d.map(|d| d.0), Some(false));
        assert_eq!(all, b"POST /");
        assert_eq!(decide(&[b"PR"]).await.0, None);
        assert_eq!(decide(&[]).await.0, None);
    }

    #[tokio::test]
    async fn a_connection_that_sends_a_partial_preface_and_closes_is_dropped() {
        use tokio::io::AsyncWriteExt;
        let origin = Origin::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let mut stream = TcpStream::connect(origin.addr()).await.unwrap();
        stream.write_all(b"PR").await.unwrap();
        stream.shutdown().await.unwrap();
        let mut rest = Vec::new();
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut rest))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 0);
        // And the origin still serves.
        assert_eq!(
            crate::client::get(origin.addr(), "/obj/3/k1", Duration::from_secs(5))
                .await
                .unwrap()
                .len(),
            3
        );
    }
}
