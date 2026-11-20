//! The upstream HTTP/2 connection pool: many requests share few
//! connections, one stream each.
//!
//! A [`Pool`] keeps, per origin authority, at most
//! [`PoolLimits::max_connections`] connections. A request goes on the open
//! connection with the fewest streams in flight while that is under
//! [`PoolLimits::max_streams_per_connection`]; when every connection is
//! that busy a new one is opened, one connect at a time per origin, and
//! once the connection limit is reached the request joins the least busy
//! connection anyway, where h2 holds it until the origin's own
//! `SETTINGS_MAX_CONCURRENT_STREAMS` lets it open. So 10,000 concurrent
//! streams to one origin with the defaults (100 streams a connection, 3
//! connections) use 3 connections, never more.
//!
//! A stream counts as in flight until its response body has been read to
//! the end or dropped ([`PoolBody`]).
//!
//! # GOAWAY
//!
//! A connection that sends `GOAWAY` is drained: no new stream is put on
//! it, streams it already accepted finish there, and it is closed once
//! they have. Streams it did not process (numbered above the `GOAWAY`'s
//! last stream ID, or reset with `REFUSED_STREAM`) fail with
//! [`PoolError::Refused`]: the origin promises it never acted on them
//! (RFC 9113 sections 6.8 and 8.7), so [`Pool::send_with`] sends them again
//! on another connection whatever their method.
//!
//! # Flow control
//!
//! The pool speaks h2 directly, so it owns its receive windows. They start
//! at the settings' windows (1 MiB a stream and 4 MiB the connection by
//! default) and, with `adaptive_window`, grow to the measured
//! bandwidth-delay product ([`super::flow::Bdp`]): when response data
//! arrives and no measurement is running, the connection sends a `PING`,
//! counts the bytes read until its `ACK`, and when that reaches two thirds
//! of the window, doubles the window (stream windows by `SETTINGS`, the
//! connection's by `WINDOW_UPDATE`), up to 16 MiB. Data is counted as the
//! reader takes it, so a slow reader never grows the windows.
//!
//! # Idle reaping
//!
//! A connection with no stream in flight for
//! [`PoolLimits::idle_timeout`] is closed by the pool's reaper, which runs
//! every quarter of that timeout until the pool is dropped.

use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Buf, Bytes};
use h2::client::SendRequest;
use h2::{RecvStream, SendStream};
use http::{Request, Response, Uri};
use hyper::body::{Body, Frame};
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::Instant;

use super::Settings;
use super::flow::{ADAPTIVE_MAX_WINDOW, Bdp, Window};
use crate::http1::{BoxError, TlsTo};

/// How the pool sizes and ages its connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolLimits {
    /// Connections kept to one origin.
    pub max_connections: usize,
    /// Streams in flight on a connection before another is opened.
    pub max_streams_per_connection: usize,
    /// How long a connection may carry no stream before it is closed.
    pub idle_timeout: Duration,
    /// How long a TCP connect, and then the TLS and HTTP/2 handshakes,
    /// may each take.
    pub connect_timeout: Duration,
    /// How many times [`Pool::send_with`] sends a refused request again.
    pub max_refused_retries: u32,
}

impl Default for PoolLimits {
    /// Three connections of 100 streams per origin, 90 seconds idle (the
    /// HTTP/1.1 client's idle timeout), a one-second connect and two
    /// resends of a refused stream.
    fn default() -> Self {
        PoolLimits {
            max_connections: 3,
            max_streams_per_connection: 100,
            idle_timeout: Duration::from_secs(90),
            connect_timeout: Duration::from_secs(1),
            max_refused_retries: 2,
        }
    }
}

/// Why a request through the pool failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolError {
    /// The connection was refused, reset, timed out or failed its TLS or
    /// HTTP/2 handshake.
    Connect,
    /// The connect took longer than `connect_timeout`.
    ConnectTimeout,
    /// The origin was reached but does not speak HTTP/2: TLS negotiated
    /// another protocol, or the HTTP/2 handshake failed.
    NotHttp2,
    /// The origin did not process the stream (GOAWAY or REFUSED_STREAM):
    /// it is safe to send again.
    Refused,
    /// The stream or connection failed after the origin may have acted on
    /// the request.
    Protocol,
}

impl PoolError {
    /// A short stable name for logs and metric labels.
    pub fn as_str(self) -> &'static str {
        match self {
            PoolError::Connect => "connect",
            PoolError::ConnectTimeout => "connect_timeout",
            PoolError::NotHttp2 => "not_http2",
            PoolError::Refused => "refused_stream",
            PoolError::Protocol => "protocol",
        }
    }

    /// Classifies a stream's failure by its h2 error.
    fn of(e: &h2::Error) -> PoolError {
        if unprocessed(e) {
            PoolError::Refused
        } else {
            PoolError::Protocol
        }
    }
}

/// Whether an h2 stream error means the origin never processed the
/// stream: a remote GOAWAY that excluded it, or its REFUSED_STREAM reset.
fn unprocessed(e: &h2::Error) -> bool {
    e.is_remote()
        && (e.is_go_away() || (e.is_reset() && e.reason() == Some(h2::Reason::REFUSED_STREAM)))
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for PoolError {}

/// Response bytes read on a connection while a measurement runs.
#[derive(Default)]
struct Received {
    bytes: AtomicU64,
    data: Notify,
}

/// One pooled connection.
struct Conn {
    id: u64,
    sender: SendRequest<Bytes>,
    streams: AtomicUsize,
    /// Set once the origin has sent GOAWAY or a stream failed with the
    /// connection.
    draining: AtomicBool,
    /// Set when the connection has ended.
    closed: Arc<AtomicBool>,
    received: Arc<Received>,
    idle_since: Mutex<Instant>,
}

impl Conn {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    fn usable(&self) -> bool {
        !self.draining.load(Ordering::Relaxed) && !self.is_closed()
    }
}

/// An origin's connections, and the lock that lets one connect run at a
/// time.
struct Origin {
    conns: Mutex<Vec<Arc<Conn>>>,
    connecting: tokio::sync::Mutex<()>,
}

struct Inner {
    limits: PoolLimits,
    settings: Settings,
    tls: Option<TlsTo>,
    origins: Mutex<HashMap<String, Arc<Origin>>>,
    next_id: AtomicU64,
    opened: AtomicUsize,
}

/// A pool of HTTP/2 connections to origins, sending request bodies of type
/// `B`. Cloning is cheap and clones share the connections.
pub struct Pool<B> {
    inner: Arc<Inner>,
    _body: PhantomData<fn(B)>,
}

impl<B> Clone for Pool<B> {
    fn clone(&self) -> Self {
        Pool {
            inner: Arc::clone(&self.inner),
            _body: PhantomData,
        }
    }
}

impl<B> fmt::Debug for Pool<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("limits", &self.inner.limits)
            .finish_non_exhaustive()
    }
}

/// A response body from the pool. Each chunk read is handed back to the
/// origin's window, and the stream counts as in flight on its connection
/// until the body ends or is dropped.
pub struct PoolBody {
    recv: RecvStream,
    data_done: bool,
    finished: bool,
    received: Arc<Received>,
    _stream: Stream,
}

impl Body for PoolBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = &mut *self;
        if this.finished {
            return Poll::Ready(None);
        }
        if !this.data_done {
            match this.recv.poll_data(cx) {
                Poll::Ready(Some(Ok(data))) => {
                    let n = data.len();
                    let _ = this.recv.flow_control().release_capacity(n);
                    this.received.bytes.fetch_add(n as u64, Ordering::Relaxed);
                    this.received.data.notify_one();
                    return Poll::Ready(Some(Ok(Frame::data(data))));
                }
                Poll::Ready(Some(Err(e))) => {
                    this.finished = true;
                    return Poll::Ready(Some(Err(e.into())));
                }
                Poll::Ready(None) => this.data_done = true,
                Poll::Pending => return Poll::Pending,
            }
        }
        match this.recv.poll_trailers(cx) {
            Poll::Ready(Ok(Some(t))) => {
                this.finished = true;
                Poll::Ready(Some(Ok(Frame::trailers(t))))
            }
            Poll::Ready(Ok(None)) => {
                this.finished = true;
                Poll::Ready(None)
            }
            Poll::Ready(Err(e)) => {
                this.finished = true;
                Poll::Ready(Some(Err(e.into())))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.finished || self.recv.is_end_stream()
    }
}

/// A stream in flight on a connection, until dropped.
struct Stream(Arc<Conn>);

impl Drop for Stream {
    fn drop(&mut self) {
        let c = &self.0;
        if c.streams.fetch_sub(1, Ordering::Relaxed) == 1 {
            *c.idle_since.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
        }
    }
}

/// Request headers HTTP/2 forbids (RFC 9113 section 8.2.2), which h2
/// refuses to send: the connection-specific ones, and `TE` other than
/// `trailers`.
fn strip_connection_headers(headers: &mut http::HeaderMap) {
    use http::header;
    for name in [
        header::CONNECTION,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
        header::HOST,
    ] {
        headers.remove(name);
    }
    headers.remove("keep-alive");
    headers.remove("proxy-connection");
    let trailers = headers
        .get_all(header::TE)
        .iter()
        .any(|v| v.as_bytes().eq_ignore_ascii_case(b"trailers"));
    headers.remove(header::TE);
    if trailers {
        headers.insert(header::TE, http::HeaderValue::from_static("trailers"));
    }
}

/// Sends a request body on its stream, as the origin's window allows.
async fn pump<B>(mut body: B, mut send: SendStream<Bytes>)
where
    B: Body + Unpin,
    B::Error: Into<BoxError>,
{
    use http_body_util::BodyExt;
    loop {
        // The body's error is dropped at once: it need not be `Send`.
        let next = body.frame().await.map(|r| r.map_err(drop));
        match next {
            None => {
                let _ = send.send_data(Bytes::new(), true);
                return;
            }
            Some(Err(_)) => {
                send.send_reset(h2::Reason::CANCEL);
                return;
            }
            Some(Ok(frame)) => match frame.into_data() {
                Ok(mut data) => {
                    let mut bytes = data.copy_to_bytes(data.remaining());
                    while !bytes.is_empty() {
                        send.reserve_capacity(bytes.len());
                        match std::future::poll_fn(|cx| send.poll_capacity(cx)).await {
                            Some(Ok(0)) => {}
                            Some(Ok(n)) => {
                                let chunk = bytes.split_to(n.min(bytes.len()));
                                if send.send_data(chunk, false).is_err() {
                                    return;
                                }
                            }
                            _ => return,
                        }
                    }
                }
                Err(frame) => {
                    if let Ok(trailers) = frame.into_trailers() {
                        let _ = send.send_trailers(trailers);
                        return;
                    }
                }
            },
        }
    }
}

/// Drives a connection until it ends, measuring the bandwidth-delay
/// product and growing the windows when `bdp` is set.
async fn drive<T>(
    mut conn: h2::client::Connection<T, Bytes>,
    received: Arc<Received>,
    closed: Arc<AtomicBool>,
    mut bdp: Option<Bdp>,
    id: u64,
) where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut pings = conn.ping_pong();
    if pings.is_none() {
        bdp = None;
    }
    let mut pinged: Option<Instant> = None;
    loop {
        tokio::select! {
            ended = &mut conn => {
                if let Err(e) = ended {
                    tracing::debug!(conn = id, error = %e, "upstream h2 connection ended");
                }
                break;
            }
            () = received.data.notified(), if bdp.is_some() && pinged.is_none() => {
                received.bytes.store(0, Ordering::Relaxed);
                if let Some(p) = pings.as_mut()
                    && p.send_ping(h2::Ping::opaque()).is_ok()
                {
                    pinged = Some(Instant::now());
                }
            }
            pong = std::future::poll_fn(|cx| match pings.as_mut() {
                Some(p) => p.poll_pong(cx),
                None => Poll::Pending,
            }), if pinged.is_some() => {
                let rtt = pinged.take().map_or(Duration::ZERO, |at| at.elapsed());
                if pong.is_err() {
                    bdp = None;
                    continue;
                }
                let bytes = received.bytes.swap(0, Ordering::Relaxed);
                if let Some(grown) = bdp.as_mut().and_then(|b| b.sample(bytes, rtt)) {
                    tracing::debug!(conn = id, window = grown.stream, rtt_ms = rtt.as_millis() as u64, "upstream h2 window grown");
                    conn.set_target_window_size(grown.connection);
                    let _ = conn.set_initial_window_size(grown.stream);
                }
            }
        }
    }
    closed.store(true, Ordering::Relaxed);
}

impl<B> Pool<B>
where
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    /// A pool over cleartext HTTP/2 (prior knowledge, h2c). Must be called
    /// inside a tokio runtime, which runs the reaper.
    pub fn new(limits: PoolLimits, settings: Settings) -> Pool<B> {
        Pool::build(limits, settings, None)
    }

    /// A pool over TLS, offering `h2` over ALPN, with `tls`'s trust and
    /// client certificate.
    pub fn with_tls(limits: PoolLimits, settings: Settings, tls: TlsTo) -> Pool<B> {
        Pool::build(limits, settings, Some(tls.with_alpn(vec![b"h2".to_vec()])))
    }

    /// Like [`with_tls`](Self::with_tls), offering `http/1.1` after `h2`,
    /// so an origin without HTTP/2 completes its handshake and the pool
    /// reports [`PoolError::NotHttp2`] instead of a TLS failure.
    pub fn with_tls_auto(limits: PoolLimits, settings: Settings, tls: TlsTo) -> Pool<B> {
        let alpn = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Pool::build(limits, settings, Some(tls.with_alpn(alpn)))
    }

    fn build(limits: PoolLimits, settings: Settings, tls: Option<TlsTo>) -> Pool<B> {
        let inner = Arc::new(Inner {
            limits,
            settings,
            tls,
            origins: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            opened: AtomicUsize::new(0),
        });
        let weak = Arc::downgrade(&inner);
        let every = (limits.idle_timeout / 4).max(Duration::from_millis(10));
        tokio::spawn(reap(weak, every));
        Pool {
            inner,
            _body: PhantomData,
        }
    }

    /// Connections open now to `authority` (`host:port`).
    pub fn connections(&self, authority: &str) -> usize {
        self.origin(authority)
            .conns
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|c| !c.is_closed())
            .count()
    }

    /// Connections opened since the pool was made.
    pub fn opened(&self) -> usize {
        self.inner.opened.load(Ordering::Relaxed)
    }

    fn origin(&self, authority: &str) -> Arc<Origin> {
        let mut map = self.inner.origins.lock().unwrap_or_else(|p| p.into_inner());
        Arc::clone(map.entry(authority.to_owned()).or_insert_with(|| {
            Arc::new(Origin {
                conns: Mutex::new(Vec::new()),
                connecting: tokio::sync::Mutex::new(()),
            })
        }))
    }

    /// The least busy usable connection with room for another stream, or
    /// with `overflow`, the least busy usable one at all.
    fn pick(&self, origin: &Origin, overflow: bool) -> Option<Arc<Conn>> {
        let mut conns = origin.conns.lock().unwrap_or_else(|p| p.into_inner());
        conns.retain(|c| !c.is_closed());
        let best = conns
            .iter()
            .filter(|c| c.usable())
            .min_by_key(|c| c.streams.load(Ordering::Relaxed))?;
        let room =
            best.streams.load(Ordering::Relaxed) < self.inner.limits.max_streams_per_connection;
        (room || overflow).then(|| Arc::clone(best))
    }

    fn usable_count(&self, origin: &Origin) -> usize {
        origin
            .conns
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|c| c.usable())
            .count()
    }

    /// A connection for one more stream to `authority`.
    async fn checkout(&self, authority: &str, host: &str) -> Result<Arc<Conn>, PoolError> {
        let origin = self.origin(authority);
        if let Some(c) = self.pick(&origin, false) {
            return Ok(c);
        }
        let _connecting = origin.connecting.lock().await;
        // Another request may have opened one while this one waited.
        if let Some(c) = self.pick(&origin, false) {
            return Ok(c);
        }
        if self.usable_count(&origin) >= self.inner.limits.max_connections
            && let Some(c) = self.pick(&origin, true)
        {
            return Ok(c);
        }
        let conn = Arc::new(self.connect(authority, host).await?);
        origin
            .conns
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Arc::clone(&conn));
        Ok(conn)
    }

    fn builder(&self) -> h2::client::Builder {
        let s = &self.inner.settings;
        let mut b = h2::client::Builder::new();
        b.initial_window_size(s.initial_stream_window)
            .initial_connection_window_size(s.initial_connection_window)
            .max_frame_size(s.max_frame_size)
            .max_header_list_size(s.max_header_list_size)
            .enable_push(false);
        b
    }

    /// The handshake on `io`, with the connection driven on its own task.
    async fn handshake<T>(&self, io: T) -> Result<Conn, PoolError>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let limit = self.inner.limits.connect_timeout;
        let (sender, conn) = tokio::time::timeout(limit, self.builder().handshake(io))
            .await
            .map_err(|_| PoolError::ConnectTimeout)?
            .map_err(|_| PoolError::NotHttp2)?;
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let received = Arc::new(Received::default());
        let closed = Arc::new(AtomicBool::new(false));
        let s = &self.inner.settings;
        let bdp = s.adaptive_window.then(|| {
            Bdp::new(
                Window {
                    stream: s.initial_stream_window,
                    connection: s.initial_connection_window,
                },
                ADAPTIVE_MAX_WINDOW,
            )
        });
        tokio::spawn(drive(
            conn,
            Arc::clone(&received),
            Arc::clone(&closed),
            bdp,
            id,
        ));
        Ok(Conn {
            id,
            sender,
            streams: AtomicUsize::new(0),
            draining: AtomicBool::new(false),
            closed,
            received,
            idle_since: Mutex::new(Instant::now()),
        })
    }

    async fn connect(&self, authority: &str, host: &str) -> Result<Conn, PoolError> {
        let limit = self.inner.limits.connect_timeout;
        let tcp = tokio::time::timeout(limit, TcpStream::connect(authority))
            .await
            .map_err(|_| PoolError::ConnectTimeout)?
            .map_err(|_| PoolError::Connect)?;
        let _ = tcp.set_nodelay(true);
        let conn = match &self.inner.tls {
            None => {
                // With prior knowledge the handshake is only the client's
                // preface; whether the origin speaks HTTP/2 shows in the
                // first frame it sends back, which must be SETTINGS.
                let (first, tap) = Tap::new(tcp);
                let conn = self.handshake(tap).await?;
                match tokio::time::timeout(limit, first).await {
                    Ok(Ok(true)) => {}
                    Ok(_) => {
                        tracing::debug!(origin = authority, "the origin did not answer in HTTP/2");
                        return Err(PoolError::NotHttp2);
                    }
                    Err(_) => return Err(PoolError::ConnectTimeout),
                }
                conn
            }
            Some(tls) => {
                let name = match tls.server_name() {
                    Some(n) => n.clone(),
                    None => ServerName::try_from(host.trim_matches(['[', ']']).to_owned())
                        .map_err(|_| PoolError::Connect)?,
                };
                let connector = tokio_rustls::TlsConnector::from(tls.client_config());
                let stream = tokio::time::timeout(limit, connector.connect(name, tcp))
                    .await
                    .map_err(|_| PoolError::ConnectTimeout)?
                    .map_err(|e| {
                        tracing::debug!(origin = authority, error = %e, "tls to the origin failed");
                        PoolError::Connect
                    })?;
                if stream.get_ref().1.alpn_protocol() != Some(b"h2") {
                    tracing::debug!(origin = authority, "the origin did not negotiate h2");
                    return Err(PoolError::NotHttp2);
                }
                self.handshake(stream).await?
            }
        };
        self.inner.opened.fetch_add(1, Ordering::Relaxed);
        Ok(conn)
    }

    /// Makes sure a connection to the origin `uri` names is open, opening
    /// one if none is, without sending anything: so a caller can learn
    /// that the origin does not speak HTTP/2 ([`PoolError::NotHttp2`])
    /// while it still holds its request.
    pub async fn prepare(&self, uri: &Uri) -> Result<(), PoolError> {
        let host = uri.host().ok_or(PoolError::Protocol)?;
        let port = uri
            .port_u16()
            .unwrap_or(if self.inner.tls.is_some() { 443 } else { 80 });
        let authority = format!("{host}:{port}");
        let origin = self.origin(&authority);
        if self.pick(&origin, true).is_some() {
            return Ok(());
        }
        self.checkout(&authority, host).await.map(drop)
    }

    /// Sends `req`, whose URI must be absolute (`http://host:port/path`;
    /// the scheme on the wire is `https` over TLS), once, and returns when
    /// the response head arrives. The request body streams on its own task
    /// as the origin's window allows.
    pub async fn send(&self, req: Request<B>) -> Result<Response<PoolBody>, PoolError> {
        let (mut parts, body) = req.into_parts();
        let uri = parts.uri.clone();
        let host = uri.host().ok_or(PoolError::Protocol)?.to_owned();
        let port = uri
            .port_u16()
            .unwrap_or(if self.inner.tls.is_some() { 443 } else { 80 });
        let authority = format!("{host}:{port}");
        // The URI names the origin to connect to; `:authority` carries the
        // request's `Host` when it has one, as HTTP/1.1's `Host` would.
        let scheme = if self.inner.tls.is_some() {
            "https"
        } else {
            "http"
        };
        let named = parts
            .headers
            .get(http::header::HOST)
            .and_then(|h| h.to_str().ok().map(str::to_owned));
        parts.uri = rebuild(&uri, scheme, named.as_deref())?;
        parts.version = http::Version::HTTP_2;
        strip_connection_headers(&mut parts.headers);
        let conn = self.checkout(&authority, &host).await?;
        conn.streams.fetch_add(1, Ordering::Relaxed);
        let stream = Stream(Arc::clone(&conn));
        let started = async {
            let mut sender = conn.sender.clone().ready().await?;
            let end = body.is_end_stream();
            let (response, send) = sender.send_request(Request::from_parts(parts, ()), end)?;
            if !end {
                tokio::spawn(pump(body, send));
            }
            response.await
        };
        match started.await {
            Ok(resp) => {
                let received = Arc::clone(&conn.received);
                Ok(resp.map(|recv| PoolBody {
                    recv,
                    data_done: false,
                    finished: false,
                    received,
                    _stream: stream,
                }))
            }
            Err(e) => {
                let err = PoolError::of(&e);
                if err == PoolError::Refused || e.is_go_away() || e.is_io() {
                    // GOAWAY: nothing new goes on this connection.
                    conn.draining.store(true, Ordering::Relaxed);
                }
                tracing::debug!(conn = conn.id, error = %e, "upstream h2 stream failed");
                Err(err)
            }
        }
    }

    /// Sends the request `make` builds, and while the origin refuses it
    /// unprocessed, builds and sends it again on another connection, up to
    /// [`PoolLimits::max_refused_retries`] times.
    pub async fn send_with(
        &self,
        make: impl Fn() -> Request<B>,
    ) -> Result<Response<PoolBody>, PoolError> {
        let mut tries = 0;
        loop {
            match self.send(make()).await {
                Err(PoolError::Refused) if tries < self.inner.limits.max_refused_retries => {
                    tries += 1;
                }
                other => return other,
            }
        }
    }
}

struct Tap<S> {
    inner: S,
    head: Vec<u8>,
    first: Option<tokio::sync::oneshot::Sender<bool>>,
}

impl<S> Tap<S> {
    fn new(inner: S) -> (tokio::sync::oneshot::Receiver<bool>, Tap<S>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tap = Tap {
            inner,
            head: Vec::with_capacity(9),
            first: Some(tx),
        };
        (rx, tap)
    }

    fn saw(&mut self, bytes: &[u8]) {
        let Some(tx) = self.first.take() else { return };
        if bytes.is_empty() {
            let _ = tx.send(false);
            return;
        }
        let take = (9 - self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..take]);
        if self.head.len() < 9 {
            self.first = Some(tx);
            return;
        }
        let h = &self.head;
        let _ = tx.send(h[3] == 0x4 && h[5..9] == [0, 0, 0, 0]);
    }
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Tap<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.inner).poll_read(cx, buf);
        match &polled {
            Poll::Ready(Ok(())) if this.first.is_some() => {
                let filled = buf.filled()[before..].to_vec();
                this.saw(&filled);
            }
            Poll::Ready(Err(_)) => {
                if let Some(tx) = this.first.take() {
                    let _ = tx.send(false);
                }
            }
            _ => {}
        }
        polled
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Tap<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// `uri` with `scheme`, and with `authority` in place of its own when
/// given.
fn rebuild(uri: &Uri, scheme: &str, authority: Option<&str>) -> Result<Uri, PoolError> {
    let mut parts = uri.clone().into_parts();
    parts.scheme = Some(scheme.parse().map_err(|_| PoolError::Protocol)?);
    if let Some(a) = authority {
        parts.authority = Some(a.parse().map_err(|_| PoolError::Protocol)?);
    }
    if parts.path_and_query.is_none() {
        parts.path_and_query = Some(http::uri::PathAndQuery::from_static("/"));
    }
    Uri::from_parts(parts).map_err(|_| PoolError::Protocol)
}

/// Closes connections idle past the timeout, every `every`, until the
/// pool is gone.
async fn reap(pool: Weak<Inner>, every: Duration) {
    loop {
        tokio::time::sleep(every).await;
        let Some(inner) = pool.upgrade() else { return };
        let origins: Vec<Arc<Origin>> = inner
            .origins
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect();
        let idle = inner.limits.idle_timeout;
        for origin in origins {
            let mut conns = origin.conns.lock().unwrap_or_else(|p| p.into_inner());
            conns.retain(|c| {
                if c.is_closed() {
                    return false;
                }
                let quiet = c.streams.load(Ordering::Relaxed) == 0;
                let since = *c.idle_since.lock().unwrap_or_else(|p| p.into_inner());
                // A dropped sender lets h2 send GOAWAY and close.
                !(quiet && since.elapsed() >= idle)
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_names() {
        let l = PoolLimits::default();
        assert_eq!(l.max_connections, 3);
        assert_eq!(l.max_streams_per_connection, 100);
        assert_eq!(l.idle_timeout, Duration::from_secs(90));
        for (e, name) in [
            (PoolError::Connect, "connect"),
            (PoolError::ConnectTimeout, "connect_timeout"),
            (PoolError::NotHttp2, "not_http2"),
            (PoolError::Refused, "refused_stream"),
            (PoolError::Protocol, "protocol"),
        ] {
            assert_eq!(e.to_string(), name);
        }
    }

    #[tokio::test]
    async fn the_tap_reads_the_first_frame_header() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (input, want) in [
            (&[0u8, 0, 6, 4, 0, 0, 0, 0, 0, 1][..], true),
            (&b"HTTP/1.1 400 Bad Request\r\n"[..], false),
            (&[0u8, 0, 0, 4, 0, 0, 0, 0, 1][..], false),
            (&[0u8, 0, 0][..], false),
        ] {
            let (mut far, near) = tokio::io::duplex(64);
            let (first, mut tap) = Tap::new(near);
            far.write_all(input).await.unwrap();
            drop(far);
            let mut sink = Vec::new();
            let _ = tap.read_to_end(&mut sink).await;
            assert_eq!(first.await, Ok(want), "{input:?}");
        }
    }

    #[test]
    fn uris_take_the_scheme_and_the_named_authority() {
        let u: Uri = "http://10.0.0.1:8443/x?y=1".parse().unwrap();
        assert_eq!(
            rebuild(&u, "https", None).unwrap().to_string(),
            "https://10.0.0.1:8443/x?y=1"
        );
        assert_eq!(
            rebuild(&u, "http", Some("api.test")).unwrap().to_string(),
            "http://api.test/x?y=1"
        );
        assert_eq!(
            rebuild(&u, "http", Some("bad host")),
            Err(PoolError::Protocol)
        );
    }
}
