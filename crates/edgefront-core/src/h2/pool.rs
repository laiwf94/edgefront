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
//! streams to one origin with the defaults (100 streams a connection, 4
//! connections) use 4 connections, never more.
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
//! # Idle reaping
//!
//! A connection with no stream in flight for
//! [`PoolLimits::idle_timeout`] is closed by the pool's reaper, which runs
//! every quarter of that timeout until the pool is dropped.

use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{Request, Response, Uri};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::client::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio::time::Instant;

use super::Settings;
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
    /// Four connections of 100 streams per origin, 90 seconds idle (the
    /// HTTP/1.1 client's idle timeout), a one-second connect and two
    /// resends of a refused stream.
    fn default() -> Self {
        PoolLimits {
            max_connections: 4,
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
            PoolError::Refused => "refused_stream",
            PoolError::Protocol => "protocol",
        }
    }

    /// Classifies a stream's failure by the h2 error behind hyper's.
    fn of(e: &hyper::Error) -> PoolError {
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(e);
        while let Some(s) = source {
            if let Some(h2) = s.downcast_ref::<h2::Error>() {
                return if unprocessed(h2) {
                    PoolError::Refused
                } else {
                    PoolError::Protocol
                };
            }
            source = s.source();
        }
        if e.is_canceled() {
            // hyper gave the request back before it was written.
            return PoolError::Refused;
        }
        PoolError::Protocol
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

/// One pooled connection.
struct Conn<B> {
    id: u64,
    sender: http2::SendRequest<B>,
    streams: AtomicUsize,
    /// Set once the origin has sent GOAWAY or the connection failed.
    draining: AtomicBool,
    idle_since: Mutex<Instant>,
}

impl<B> Conn<B> {
    fn usable(&self) -> bool {
        !self.draining.load(Ordering::Relaxed) && !self.sender.is_closed()
    }
}

/// An origin's connections, and the lock that lets one connect run at a
/// time.
struct Origin<B> {
    conns: Mutex<Vec<Arc<Conn<B>>>>,
    connecting: tokio::sync::Mutex<()>,
}

struct Inner<B> {
    limits: PoolLimits,
    settings: Settings,
    tls: Option<TlsTo>,
    origins: Mutex<HashMap<String, Arc<Origin<B>>>>,
    next_id: AtomicUsize,
    opened: AtomicUsize,
}

/// A pool of HTTP/2 connections to origins. Cloning is cheap and clones
/// share the connections.
pub struct Pool<B> {
    inner: Arc<Inner<B>>,
}

impl<B> Clone for Pool<B> {
    fn clone(&self) -> Self {
        Pool {
            inner: Arc::clone(&self.inner),
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

/// A response body from the pool. It keeps its stream counted as in
/// flight on its connection until it ends or is dropped.
pub struct PoolBody {
    inner: Incoming,
    _stream: Stream,
}

impl Body for PoolBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// A stream in flight on a connection, until dropped.
struct Stream(Arc<dyn StreamCount>);

trait StreamCount: Send + Sync {
    fn done(&self);
}

impl<B: Send + 'static> StreamCount for Conn<B> {
    fn done(&self) {
        if self.streams.fetch_sub(1, Ordering::Relaxed) == 1 {
            *self.idle_since.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.0.done();
    }
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

    fn build(limits: PoolLimits, settings: Settings, tls: Option<TlsTo>) -> Pool<B> {
        let inner = Arc::new(Inner {
            limits,
            settings,
            tls,
            origins: Mutex::new(HashMap::new()),
            next_id: AtomicUsize::new(0),
            opened: AtomicUsize::new(0),
        });
        let weak = Arc::downgrade(&inner);
        let every = (limits.idle_timeout / 4).max(Duration::from_millis(10));
        tokio::spawn(reap(weak, every));
        Pool { inner }
    }

    /// Connections open now to `authority` (`host:port`).
    pub fn connections(&self, authority: &str) -> usize {
        self.origin(authority)
            .conns
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|c| !c.sender.is_closed())
            .count()
    }

    /// Connections opened since the pool was made.
    pub fn opened(&self) -> usize {
        self.inner.opened.load(Ordering::Relaxed)
    }

    fn origin(&self, authority: &str) -> Arc<Origin<B>> {
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
    fn pick(&self, origin: &Origin<B>, overflow: bool) -> Option<Arc<Conn<B>>> {
        let mut conns = origin.conns.lock().unwrap_or_else(|p| p.into_inner());
        conns.retain(|c| !c.sender.is_closed());
        let best = conns
            .iter()
            .filter(|c| c.usable())
            .min_by_key(|c| c.streams.load(Ordering::Relaxed))?;
        let room =
            best.streams.load(Ordering::Relaxed) < self.inner.limits.max_streams_per_connection;
        (room || overflow).then(|| Arc::clone(best))
    }

    fn usable_count(&self, origin: &Origin<B>) -> usize {
        origin
            .conns
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|c| c.usable())
            .count()
    }

    /// A connection for one more stream to `authority`.
    async fn checkout(&self, authority: &str, host: &str) -> Result<Arc<Conn<B>>, PoolError> {
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

    async fn connect(&self, authority: &str, host: &str) -> Result<Conn<B>, PoolError> {
        let limit = self.inner.limits.connect_timeout;
        let tcp = tokio::time::timeout(limit, TcpStream::connect(authority))
            .await
            .map_err(|_| PoolError::ConnectTimeout)?
            .map_err(|_| PoolError::Connect)?;
        let _ = tcp.set_nodelay(true);
        let builder = self.builder();
        let sender = match &self.inner.tls {
            None => {
                let (sender, conn) =
                    tokio::time::timeout(limit, builder.handshake(TokioIo::new(tcp)))
                        .await
                        .map_err(|_| PoolError::ConnectTimeout)?
                        .map_err(|_| PoolError::Connect)?;
                tokio::spawn(conn);
                sender
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
                    return Err(PoolError::Connect);
                }
                let (sender, conn) =
                    tokio::time::timeout(limit, builder.handshake(TokioIo::new(stream)))
                        .await
                        .map_err(|_| PoolError::ConnectTimeout)?
                        .map_err(|_| PoolError::Connect)?;
                tokio::spawn(conn);
                sender
            }
        };
        self.inner.opened.fetch_add(1, Ordering::Relaxed);
        Ok(Conn {
            id: self.inner.next_id.fetch_add(1, Ordering::Relaxed) as u64,
            sender,
            streams: AtomicUsize::new(0),
            draining: AtomicBool::new(false),
            idle_since: Mutex::new(Instant::now()),
        })
    }

    fn builder(&self) -> http2::Builder<TokioExecutor> {
        let s = &self.inner.settings;
        let mut b = http2::Builder::new(TokioExecutor::new());
        b.timer(TokioTimer::new())
            .adaptive_window(false)
            .initial_stream_window_size(s.initial_stream_window)
            .initial_connection_window_size(s.initial_connection_window)
            .max_frame_size(s.max_frame_size)
            .max_header_list_size(s.max_header_list_size);
        b
    }

    /// Sends `req`, whose URI must be absolute (`http://host:port/path`;
    /// the scheme on the wire is `https` over TLS), once, and returns when
    /// the response head arrives.
    pub async fn send(&self, mut req: Request<B>) -> Result<Response<PoolBody>, PoolError> {
        let uri = req.uri().clone();
        let host = uri.host().ok_or(PoolError::Protocol)?.to_owned();
        let port = uri
            .port_u16()
            .unwrap_or(if self.inner.tls.is_some() { 443 } else { 80 });
        let authority = format!("{}:{port}", uri.authority().map_or("", |a| a.host()));
        if self.inner.tls.is_some() {
            *req.uri_mut() = with_scheme(&uri, "https")?;
        }
        *req.version_mut() = http::Version::HTTP_2;
        // HTTP/2 carries the host in :authority.
        req.headers_mut().remove(http::header::HOST);
        let conn = self.checkout(&authority, &host).await?;
        conn.streams.fetch_add(1, Ordering::Relaxed);
        let stream = Stream(Arc::clone(&conn) as Arc<dyn StreamCount>);
        let mut sender = conn.sender.clone();
        let sent = async {
            sender.ready().await?;
            sender.send_request(req).await
        };
        match sent.await {
            Ok(resp) => Ok(resp.map(|inner| PoolBody {
                inner,
                _stream: stream,
            })),
            Err(e) => {
                let err = PoolError::of(&e);
                if err == PoolError::Refused || conn.sender.is_closed() {
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

fn with_scheme(uri: &Uri, scheme: &str) -> Result<Uri, PoolError> {
    let mut parts = uri.clone().into_parts();
    parts.scheme = Some(scheme.parse().map_err(|_| PoolError::Protocol)?);
    Uri::from_parts(parts).map_err(|_| PoolError::Protocol)
}

/// Closes connections idle past the timeout, every `every`, until the
/// pool is gone.
async fn reap<B>(pool: Weak<Inner<B>>, every: Duration) {
    loop {
        tokio::time::sleep(every).await;
        let Some(inner) = pool.upgrade() else { return };
        let origins: Vec<Arc<Origin<B>>> = inner
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
                if c.sender.is_closed() {
                    return false;
                }
                let quiet = c.streams.load(Ordering::Relaxed) == 0;
                let since = *c.idle_since.lock().unwrap_or_else(|p| p.into_inner());
                // A dropped sender lets hyper send GOAWAY and close.
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
        assert_eq!(l.max_connections, 4);
        assert_eq!(l.max_streams_per_connection, 100);
        assert_eq!(l.idle_timeout, Duration::from_secs(90));
        for (e, name) in [
            (PoolError::Connect, "connect"),
            (PoolError::ConnectTimeout, "connect_timeout"),
            (PoolError::Refused, "refused_stream"),
            (PoolError::Protocol, "protocol"),
        ] {
            assert_eq!(e.to_string(), name);
        }
    }

    #[test]
    fn schemes_are_rewritten_for_tls() {
        let u: Uri = "http://a.test:8443/x?y=1".parse().unwrap();
        assert_eq!(
            with_scheme(&u, "https").unwrap().to_string(),
            "https://a.test:8443/x?y=1"
        );
    }
}
