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

use crate::http1::{BoxError, HoldBody, server_builder};

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
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut conns = JoinSet::new();
    let mut report = ServeReport::default();
    let mut shutdown = pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    report.accepted += 1;
                    let _ = stream.set_nodelay(true);
                    conns.spawn(connection(stream, peer, handler.clone(), limits, stop_rx.clone()));
                }
                Err(e) => {
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

/// The state a connection's watchdog reads. Times are nanoseconds since
/// the accept; for `head_since`, plus one, with zero meaning "no request
/// head started since the connection went idle".
struct Watch {
    start: Instant,
    busy: AtomicU32,
    idle_since: AtomicU64,
    head_since: AtomicU64,
    wake: Notify,
}

impl Watch {
    fn new() -> Self {
        Watch {
            start: Instant::now(),
            busy: AtomicU32::new(0),
            idle_since: AtomicU64::new(0),
            // The first request's head timer runs from the accept.
            head_since: AtomicU64::new(1),
            wake: Notify::new(),
        }
    }

    fn now(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX - 1)
    }

    fn at(&self, nanos: u64) -> Instant {
        self.start + Duration::from_nanos(nanos)
    }

    /// Called for every successful read of `n` bytes.
    fn on_read(&self, n: usize) {
        if n > 0
            && self.busy.load(Ordering::Relaxed) == 0
            && self.head_since.load(Ordering::Relaxed) == 0
        {
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

/// A TCP stream that reports reads to its watchdog.
struct Watched {
    stream: TcpStream,
    watch: Arc<Watch>,
}

impl AsyncRead for Watched {
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

impl AsyncWrite for Watched {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write_vectored(cx, bufs)
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
async fn connection<H: Handler>(
    stream: TcpStream,
    peer: SocketAddr,
    handler: Arc<H>,
    limits: ServerLimits,
    mut stop: watch::Receiver<bool>,
) {
    let watch = Arc::new(Watch::new());
    let service = {
        let watch = watch.clone();
        service_fn(move |req| {
            let busy = Busy::new(watch.clone());
            let handler = handler.clone();
            async move {
                let resp = handler.call(req, peer).await;
                Ok::<_, Infallible>(resp.map(|body| HoldBody::new(body, busy)))
            }
        })
    };
    let io = TokioIo::new(Watched {
        stream,
        watch: watch.clone(),
    });
    let mut conn = pin!(
        server_builder(limits.max_header_bytes, limits.max_headers).serve_connection(io, service)
    );
    // The receiver was cloned before any stop was sent, so a stop that
    // came before this task first ran still shows up as a change.
    let mut stopping = false;
    loop {
        let deadline = watch.deadline(&limits);
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
                conn.as_mut().graceful_shutdown();
            }
            () = watch.wake.notified() => {}
            () = timer => {
                if watch.deadline(&limits).is_some_and(|at| at <= Instant::now()) {
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
