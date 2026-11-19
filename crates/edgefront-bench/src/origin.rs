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

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, Method, Request, Response, StatusCode, header};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

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
            let service = service_fn(move |request: Request<Incoming>| {
                let counters = Arc::clone(&counters);
                async move { Ok::<_, Infallible>(answer(&counters, &request, delay).await) }
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
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

/// A body of known length: a fixed buffer, or `remaining` bytes of the
/// pattern in frames of at most 1 MiB.
pub struct ObjectBody {
    fixed: Option<Bytes>,
    remaining: u64,
}

impl ObjectBody {
    fn fixed(bytes: Bytes) -> Self {
        ObjectBody {
            remaining: bytes.len() as u64,
            fixed: Some(bytes),
        }
    }

    fn empty() -> Self {
        ObjectBody {
            fixed: None,
            remaining: 0,
        }
    }

    fn pattern(size: u64) -> Self {
        ObjectBody {
            fixed: None,
            remaining: size,
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
        if let Some(bytes) = self.fixed.take() {
            self.remaining = 0;
            return Poll::Ready((!bytes.is_empty()).then(|| Ok(Frame::data(bytes))));
        }
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        let take = self.remaining.min(FRAME as u64) as usize;
        self.remaining -= take as u64;
        Poll::Ready(Some(Ok(Frame::data(pattern().slice(..take)))))
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
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
    use http_body_util::BodyExt;

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
}
