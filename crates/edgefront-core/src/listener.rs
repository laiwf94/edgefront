//! Accepting HTTP/1.1 connections with limits, keep-alive and graceful
//! shutdown.
//!
//! Each connection runs on its own task with a small watchdog that enforces
//! two timeouts the way Go's `net/http` server, and so steer, does:
//!
//! - the header read timeout runs from the accept for the first request,
//!   and from the first byte of each later request head;
//! - the idle timeout runs from the end of a response until the first byte
//!   of the next request.
//!
//! A request counts as in flight from the moment its head is parsed until
//! hyper has written and dropped its response body, so neither timeout can
//! cut a slow handler, a long upload or a streamed response. Bytes the
//! client pipelines while a request is in flight do not start the header
//! timer; such a connection is bounded by the idle timeout instead.
//!
//! On shutdown the listener stops accepting, closes idle connections,
//! lets each busy connection finish its current request (answered with
//! `Connection: close`), and after the drain timeout closes whatever is
//! left.
//!
//! # HTTP/2
//!
//! With [`ServerLimits::http2`] set, a TLS connection that negotiated `h2`
//! over ALPN, and, when the settings allow cleartext, a plain connection
//! whose first 24 bytes are the HTTP/2 preface, is served as HTTP/2 with
//! those settings ([`crate::h2`]). Each stream is a request for the same
//! handler. The header read timeout then bounds the preface, and the idle
//! timeout runs whenever no stream is open; frames on an idle connection
//! (`PING`, `SETTINGS`) do not start a header timer. Shutdown sends
//! `GOAWAY`, lets open streams finish within the drain timeout, and
//! refuses new ones.

use std::convert::Infallible;
use std::future::{Future, pending};
use std::io;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{Request, Response};
use hyper::body::{Body, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::h2::server::{Rewind, sniff_preface};
use crate::http1::{BoxError, HoldBody, server_builder};
use crate::tls::{Acceptor, Slot, TlsInfo};

/// Limits for every connection a listener accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLimits {
    /// Time a client has to send a complete request head.
    pub header_read_timeout: Duration,
    /// Time a keep-alive connection may sit between requests.
    pub idle_timeout: Duration,
    /// Largest request head (request line and headers, with their line
    /// ends) accepted; a larger one gets 431 and the connection closes.
    pub max_header_bytes: usize,
    /// Most header fields in one request head; more gets 431.
    pub max_headers: usize,
    /// How long shutdown waits for in-flight requests.
    pub drain_timeout: Duration,
    /// HTTP/2 for clients that negotiate it; `None` serves HTTP/1.1 only.
    pub http2: Option<crate::h2::Settings>,
}

/// Per-connection quotas: once a connection has carried `max_requests`
/// requests, or `max_bytes` bytes read and written, the request that
/// reaches the quota is the last. Its HTTP/1.1 response says
/// `Connection: close`, an HTTP/2 connection gets GOAWAY so the client
/// opens a new one for its next streams, and requests already in flight
/// finish. `None` sets no quota. A [`Handler`] gives its own with
/// [`Handler::conn_quota`]; the limiter's
/// `edgefront_limiter::conn_quota` reads these from its config.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConnQuota {
    pub max_requests: Option<u64>,
    pub max_bytes: Option<u64>,
}

impl Default for ServerLimits {
    /// steer's proxy listener: 10 seconds for the head, 120 seconds idle,
    /// 64 KiB of head, and 30 seconds to drain. hyper's default of 100
    /// header fields stands in for Go, which has no count limit.
    fn default() -> Self {
        ServerLimits {
            header_read_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(120),
            max_header_bytes: 64 << 10,
            max_headers: 100,
            drain_timeout: Duration::from_secs(30),
            http2: None,
        }
    }
}

/// Something that answers requests.
pub trait Handler: Send + Sync + 'static {
    /// The response body. `Unpin` so the listener can wrap it without
    /// boxing; box a body that is not.
    type Body: Body<Data = Bytes, Error: Into<BoxError>> + Send + Unpin + 'static;

    fn call(
        self: Arc<Self>,
        req: Request<Incoming>,
        peer: SocketAddr,
    ) -> impl Future<Output = Response<Self::Body>> + Send;

    /// The quota each connection to this handler gets; none by default.
    fn conn_quota(&self) -> ConnQuota {
        ConnQuota::default()
    }
}

/// What happened while a listener ran.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ServeReport {
    /// Connections accepted.
    pub accepted: u64,
    /// Connections still open when the drain timeout ran out, which were
    /// then closed.
    pub forced: usize,
}

/// Serves `handler` on `listener` until `shutdown` resolves, then drains
/// as the module documentation describes.
pub async fn serve<H: Handler>(
    listener: TcpListener,
    handler: Arc<H>,
    limits: ServerLimits,
    shutdown: impl Future<Output = ()>,
) -> ServeReport {
    run(listener, None, handler, limits, shutdown).await
}

/// Like [`serve`], with every connection's TLS handshake run by
/// `acceptor` first, off the accept loop.
///
/// The loop takes a handshake slot before it accepts, so at most the
/// current snapshot's `max_concurrent_handshakes` run at once and later
/// connections wait in the kernel's backlog. A connection whose handshake
/// fails or times out is closed and never reaches `handler`. The header
/// read timeout starts once the handshake is done. Each request carries
/// the connection's [`TlsInfo`](crate::tls::TlsInfo) as an
/// `Arc<TlsInfo>` extension. On shutdown, handshakes still running are
/// abandoned and not reported.
pub async fn serve_tls<H: Handler>(
    listener: TcpListener,
    acceptor: Arc<Acceptor>,
    handler: Arc<H>,
    limits: ServerLimits,
    shutdown: impl Future<Output = ()>,
) -> ServeReport {
    run(listener, Some(acceptor), handler, limits, shutdown).await
}

async fn run<H: Handler>(
    listener: TcpListener,
    acceptor: Option<Arc<Acceptor>>,
    handler: Arc<H>,
    limits: ServerLimits,
    shutdown: impl Future<Output = ()>,
) -> ServeReport {
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut conns = JoinSet::new();
    let mut report = ServeReport::default();
    let mut shutdown = pin!(shutdown);
    'serving: loop {
        // With TLS, a handshake slot first; reaping finished connections
        // and watching for shutdown while waiting for one.
        let slot = match &acceptor {
            None => None,
            Some(a) => loop {
                tokio::select! {
                    slot = a.slot() => break Some(slot),
                    Some(_) = conns.join_next(), if !conns.is_empty() => {}
                    () = &mut shutdown => break 'serving,
                }
            },
        };
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    report.accepted += 1;
                    let _ = stream.set_nodelay(true);
                    let (handler, stop) = (handler.clone(), stop_rx.clone());
                    match (&acceptor, slot) {
                        (Some(a), Some(slot)) => {
                            conns.spawn(handshake_then_serve(a.clone(), slot, stream, peer, handler, limits, stop));
                        }
                        _ => {
                            conns.spawn(plain_then_serve(stream, peer, handler, limits, stop));
                        }
                    }
                }
                Err(e) => {
                    drop(slot);
                    // Usually out of file descriptors; back off instead of spinning.
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            () = &mut shutdown => break,
        }
    }
    drop(listener);
    let _ = stop_tx.send(true);
    let drained = tokio::time::timeout(limits.drain_timeout, async {
        while conns.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        report.forced = conns.len();
        tracing::warn!(
            open = report.forced,
            "shutdown drain timed out; closing remaining connections"
        );
        conns.shutdown().await;
    }
    report
}

/// Runs a connection's handshake, unless shutdown comes first, then
/// serves it.
async fn handshake_then_serve<H: Handler>(
    acceptor: Arc<Acceptor>,
    slot: Slot,
    stream: TcpStream,
    peer: SocketAddr,
    handler: Arc<H>,
    limits: ServerLimits,
    mut stop: watch::Receiver<bool>,
) {
    let established = tokio::select! {
        done = acceptor.handshake(slot, stream) => done,
        _ = stop.wait_for(|s| *s) => return,
    };
    if let Some((tls, info)) = established {
        // A TLS-ALPN-01 validation ends with its handshake (RFC 8737
        // section 3); nothing is served on acme-tls/1.
        if info.alpn.as_deref() == Some(crate::acme::challenges::ACME_TLS_ALPN) {
            return;
        }
        match limits.http2 {
            Some(settings) if info.alpn.as_deref() == Some(crate::h2::ALPN_H2) => {
                h2_over_tls(tls, peer, info, handler, limits, settings, stop).await;
            }
            _ => connection(tls, peer, Some(info), handler, limits, Proto::Http1, stop).await,
        }
    }
}

/// Serves a TLS connection that negotiated `h2`, once its preface has
/// arrived within the header read timeout. A client that sends anything
/// else gets `GOAWAY` with `PROTOCOL_ERROR` and a TLS close (RFC 9113
/// section 3.4), rather than a connection cut without `close_notify`.
async fn h2_over_tls<S, H: Handler>(
    mut tls: S,
    peer: SocketAddr,
    info: Arc<TlsInfo>,
    handler: Arc<H>,
    limits: ServerLimits,
    settings: crate::h2::Settings,
    mut stop: watch::Receiver<bool>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let sniffed = tokio::select! {
        r = tokio::time::timeout(limits.header_read_timeout, sniff_preface(&mut tls)) => r,
        _ = stop.wait_for(|s| *s) => return,
    };
    match sniffed {
        Ok(Ok((true, seen))) => {
            let stream = Rewind::new(seen, tls);
            connection(
                stream,
                peer,
                Some(info),
                handler,
                limits,
                Proto::Http2(settings),
                stop,
            )
            .await;
        }
        Ok(Ok((false, _))) => {
            tracing::debug!(peer = %peer, "invalid http/2 connection preface");
            let _ = crate::h2::server::refuse_preface(&mut tls).await;
        }
        _ => tracing::debug!(peer = %peer, "connection closed or timed out before the preface"),
    }
}

/// Serves a cleartext connection: as HTTP/2 when cleartext HTTP/2 is on
/// and the client opens with the preface (read within the header read
/// timeout), otherwise as HTTP/1.1.
async fn plain_then_serve<H: Handler>(
    mut stream: TcpStream,
    peer: SocketAddr,
    handler: Arc<H>,
    limits: ServerLimits,
    mut stop: watch::Receiver<bool>,
) {
    let Some(settings) = limits.http2.filter(|s| s.cleartext) else {
        connection(stream, peer, None, handler, limits, Proto::Http1, stop).await;
        return;
    };
    let sniffed = tokio::select! {
        r = tokio::time::timeout(limits.header_read_timeout, sniff_preface(&mut stream)) => r,
        _ = stop.wait_for(|s| *s) => return,
    };
    let Ok(Ok((h2, seen))) = sniffed else {
        tracing::debug!(peer = %peer, "connection closed or timed out before a request");
        return;
    };
    let proto = if h2 {
        Proto::Http2(settings)
    } else {
        Proto::Http1
    };
    let stream = Rewind::new(seen, stream);
    connection(stream, peer, None, handler, limits, proto, stop).await;
}

/// The protocol a connection is served with.
#[derive(Clone, Copy)]
enum Proto {
    Http1,
    Http2(crate::h2::Settings),
}

/// The state a connection's watchdog reads. Times are nanoseconds since
/// the accept; for `head_since`, plus one, with zero meaning "no request
/// head started since the connection went idle".
struct Watch {
    start: Instant,
    /// HTTP/2: reads on an idle connection are frames, not request heads.
    h2: bool,
    busy: AtomicU32,
    idle_since: AtomicU64,
    head_since: AtomicU64,
    wake: Notify,
    /// Requests started and bytes carried, for the connection quota.
    requests: AtomicU64,
    bytes: AtomicU64,
    /// The quota is spent: finish what is in flight and close.
    spent: std::sync::atomic::AtomicBool,
}

impl Watch {
    #[cfg(test)]
    fn new() -> Self {
        Watch::with_h2(false)
    }

    fn with_h2(h2: bool) -> Self {
        Watch {
            start: Instant::now(),
            h2,
            busy: AtomicU32::new(0),
            idle_since: AtomicU64::new(0),
            // The first request's head timer runs from the accept.
            head_since: AtomicU64::new(1),
            wake: Notify::new(),
            requests: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            spent: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn now(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX - 1)
    }

    fn at(&self, nanos: u64) -> Instant {
        self.start + Duration::from_nanos(nanos)
    }

    /// A request starts: whether it is the last the quota allows.
    fn last_request(&self, quota: &ConnQuota) -> bool {
        let n = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
        let bytes = self.bytes.load(Ordering::Relaxed);
        let last = quota.max_requests.is_some_and(|m| n >= m)
            || quota.max_bytes.is_some_and(|m| bytes >= m);
        if last && !self.spent.swap(true, Ordering::Relaxed) {
            self.wake.notify_one();
        }
        last
    }

    /// Called for every successful read of `n` bytes.
    fn on_read(&self, n: usize) {
        self.bytes.fetch_add(n as u64, Ordering::Relaxed);
        if n == 0 || self.busy.load(Ordering::Relaxed) > 0 {
            return;
        }
        if self.h2 {
            // The preface (or any frame) ends the first head timer; from
            // then on only the idle timeout applies between streams.
            if self.head_since.swap(0, Ordering::Relaxed) > 0 {
                self.idle_since.store(self.now(), Ordering::Relaxed);
                self.wake.notify_one();
            }
            return;
        }
        if self.head_since.load(Ordering::Relaxed) == 0 {
            self.head_since.store(self.now() + 1, Ordering::Relaxed);
            self.wake.notify_one();
        }
    }

    /// When the connection should be closed, or `None` while a request is
    /// in flight.
    fn deadline(&self, limits: &ServerLimits) -> Option<Instant> {
        if self.busy.load(Ordering::Relaxed) > 0 {
            return None;
        }
        let head = self.head_since.load(Ordering::Relaxed);
        Some(if head > 0 {
            self.at(head - 1) + limits.header_read_timeout
        } else {
            self.at(self.idle_since.load(Ordering::Relaxed)) + limits.idle_timeout
        })
    }
}

/// Marks a request in flight until dropped.
struct Busy(Arc<Watch>);

impl Busy {
    fn new(watch: Arc<Watch>) -> Self {
        watch.busy.fetch_add(1, Ordering::Relaxed);
        watch.head_since.store(0, Ordering::Relaxed);
        Busy(watch)
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        let w = &self.0;
        if w.busy.fetch_sub(1, Ordering::Relaxed) == 1 {
            w.idle_since.store(w.now(), Ordering::Relaxed);
            w.head_since.store(0, Ordering::Relaxed);
            w.wake.notify_one();
        }
    }
}

/// A stream that reports reads to its watchdog.
struct Watched<S> {
    stream: S,
    watch: Arc<Watch>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Watched<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.stream).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = polled {
            this.watch.on_read(buf.filled().len() - before);
        }
        polled
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Watched<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.stream).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = polled {
            this.watch.bytes.fetch_add(n as u64, Ordering::Relaxed);
        }
        polled
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.stream).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(n)) = polled {
            this.watch.bytes.fetch_add(n as u64, Ordering::Relaxed);
        }
        polled
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

/// Serves one connection until it ends, a timeout closes it, or shutdown
/// lets it finish.
async fn connection<S, H: Handler>(
    stream: S,
    peer: SocketAddr,
    tls: Option<Arc<TlsInfo>>,
    handler: Arc<H>,
    limits: ServerLimits,
    proto: Proto,
    mut stop: watch::Receiver<bool>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let watch = Arc::new(Watch::with_h2(matches!(proto, Proto::Http2(_))));
    let service = {
        let watch = watch.clone();
        let quota = handler.conn_quota();
        let http1 = matches!(proto, Proto::Http1);
        service_fn(move |mut req: Request<Incoming>| {
            let last = watch.last_request(&quota);
            let busy = Busy::new(watch.clone());
            if let Some(info) = &tls {
                req.extensions_mut().insert(Arc::clone(info));
            }
            let handler = handler.clone();
            async move {
                let mut resp = handler.call(req, peer).await;
                if last && http1 {
                    resp.headers_mut().insert(
                        http::header::CONNECTION,
                        http::HeaderValue::from_static("close"),
                    );
                }
                Ok::<_, Infallible>(resp.map(|body| HoldBody::new(body, busy)))
            }
        })
    };
    let io = TokioIo::new(Watched {
        stream,
        watch: watch.clone(),
    });
    match proto {
        Proto::Http1 => {
            let conn = server_builder(limits.max_header_bytes, limits.max_headers)
                .serve_connection(io, service);
            drive(conn, peer, &watch, &limits, &mut stop).await;
        }
        Proto::Http2(settings) => {
            let conn = settings.builder().serve_connection(io, service);
            drive(conn, peer, &watch, &limits, &mut stop).await;
        }
    }
}

/// A served connection of either protocol: it ends, or is asked to finish
/// what it has and stop.
trait Served: Future<Output = Result<(), hyper::Error>> {
    fn finish(self: Pin<&mut Self>);
}

impl<I, S> Served for hyper::server::conn::http1::Connection<I, S>
where
    S: hyper::service::HttpService<Incoming>,
    S::Error: Into<BoxError>,
    I: hyper::rt::Read + hyper::rt::Write + Unpin,
    S::ResBody: Body + 'static,
    <S::ResBody as Body>::Error: Into<BoxError>,
{
    fn finish(self: Pin<&mut Self>) {
        self.graceful_shutdown();
    }
}

impl<I, S, E> Served for hyper::server::conn::http2::Connection<I, S, E>
where
    S: hyper::service::HttpService<Incoming>,
    S::Error: Into<BoxError>,
    I: hyper::rt::Read + hyper::rt::Write + Unpin,
    S::ResBody: Body + 'static,
    <S::ResBody as Body>::Error: Into<BoxError>,
    E: hyper::rt::bounds::Http2ServerConnExec<S::Future, S::ResBody>,
{
    fn finish(self: Pin<&mut Self>) {
        self.graceful_shutdown();
    }
}

/// Runs a connection under its watchdog until it ends.
async fn drive<C: Served>(
    conn: C,
    peer: SocketAddr,
    watch: &Watch,
    limits: &ServerLimits,
    stop: &mut watch::Receiver<bool>,
) {
    let mut conn = pin!(conn);
    // The receiver was cloned before any stop was sent, so a stop that
    // came before this task first ran still shows up as a change.
    let mut stopping = false;
    loop {
        let deadline = watch.deadline(limits);
        let timer = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => pending().await,
            }
        };
        tokio::select! {
            ended = conn.as_mut() => {
                if let Err(e) = ended {
                    tracing::debug!(peer = %peer, error = %e, "connection ended with an error");
                }
                return;
            }
            _ = stop.changed(), if !stopping => {
                stopping = true;
                conn.as_mut().finish();
            }
            () = watch.wake.notified() => {
                // The connection quota is spent: GOAWAY for HTTP/2, no
                // more keep-alive for HTTP/1.1, after what is in flight.
                if !stopping && watch.spent.load(Ordering::Relaxed) {
                    stopping = true;
                    conn.as_mut().finish();
                }
            }
            () = timer => {
                if watch.deadline(limits).is_some_and(|at| at <= Instant::now()) {
                    tracing::debug!(peer = %peer, "connection timed out");
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> ServerLimits {
        ServerLimits {
            header_read_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(5),
            ..ServerLimits::default()
        }
    }

    #[test]
    fn defaults_match_steer() {
        let l = ServerLimits::default();
        assert_eq!(l.header_read_timeout, Duration::from_secs(10));
        assert_eq!(l.idle_timeout, Duration::from_secs(120));
        assert_eq!(l.max_header_bytes, 65536);
        assert_eq!(l.drain_timeout, Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn first_head_timer_runs_from_accept() {
        let w = Watch::new();
        assert_eq!(
            w.deadline(&limits()),
            Some(w.start + Duration::from_secs(1))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_then_head_then_busy() {
        let w = Arc::new(Watch::new());
        let busy = Busy::new(w.clone());
        assert_eq!(w.deadline(&limits()), None);
        // Reads during a request do not start a head timer.
        w.on_read(10);
        tokio::time::advance(Duration::from_secs(3)).await;
        drop(busy);
        let idle_at = w.start + Duration::from_secs(3);
        assert_eq!(
            w.deadline(&limits()),
            Some(idle_at + Duration::from_secs(5))
        );
        // An empty read (EOF) is not the start of a head.
        w.on_read(0);
        assert_eq!(
            w.deadline(&limits()),
            Some(idle_at + Duration::from_secs(5))
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        w.on_read(1);
        assert_eq!(
            w.deadline(&limits()),
            Some(w.start + Duration::from_secs(6))
        );
        // Later bytes of the same head do not move the timer.
        tokio::time::advance(Duration::from_millis(500)).await;
        w.on_read(1);
        assert_eq!(
            w.deadline(&limits()),
            Some(w.start + Duration::from_secs(6))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn http2_frames_on_an_idle_connection_are_not_heads() {
        let w = Arc::new(Watch::with_h2(true));
        assert_eq!(
            w.deadline(&limits()),
            Some(w.start + Duration::from_secs(1))
        );
        tokio::time::advance(Duration::from_millis(200)).await;
        // The preface ends the first head timer; idle runs from here.
        w.on_read(24);
        let idle_at = w.start + Duration::from_millis(200);
        assert_eq!(
            w.deadline(&limits()),
            Some(idle_at + Duration::from_secs(5))
        );
        // A PING later does not start a header timer.
        tokio::time::advance(Duration::from_secs(2)).await;
        w.on_read(17);
        assert_eq!(
            w.deadline(&limits()),
            Some(idle_at + Duration::from_secs(5))
        );
        let busy = Busy::new(w.clone());
        assert_eq!(w.deadline(&limits()), None);
        drop(busy);
        assert_eq!(
            w.deadline(&limits()),
            Some(w.start + Duration::from_millis(2200) + Duration::from_secs(5))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn overlapping_requests_stay_busy_until_the_last_ends() {
        let w = Arc::new(Watch::new());
        let a = Busy::new(w.clone());
        let b = Busy::new(w.clone());
        drop(a);
        assert_eq!(w.deadline(&limits()), None);
        drop(b);
        assert!(w.deadline(&limits()).is_some());
    }
}
