//! Accepting HTTP/1.1 connections, with limits and graceful shutdown.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use http::{Request, Response};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;

use crate::respond::Body;

#[derive(Debug, Clone, Copy)]
pub struct ServerLimits {
    /// Time a client has to send its request headers.
    pub header_read: Duration,
    /// Largest request head (line and headers) accepted.
    pub max_header_bytes: usize,
    /// How long shutdown waits for open requests.
    pub drain: Duration,
}

/// Something that answers requests.
pub trait Handler: Send + Sync + 'static {
    fn call(
        self: Arc<Self>,
        req: Request<Incoming>,
        peer: SocketAddr,
    ) -> impl Future<Output = Response<Body>> + Send;
}

/// Serves `handler` on `listener` until `shutdown` resolves, then stops
/// accepting and waits up to `limits.drain` for open connections.
pub async fn serve<H: Handler>(
    listener: TcpListener,
    handler: Arc<H>,
    limits: ServerLimits,
    shutdown: impl Future<Output = ()>,
) {
    let graceful = GracefulShutdown::new();
    tokio::pin!(shutdown);
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(e) => {
                    // Usually out of file descriptors; back off instead of spinning.
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
            () = &mut shutdown => break,
        };
        let _ = stream.set_nodelay(true);
        let handler = handler.clone();
        let service = service_fn(move |req| {
            let handler = handler.clone();
            async move { Ok::<_, Infallible>(handler.call(req, peer).await) }
        });
        let conn = http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(limits.header_read)
            .max_buf_size(limits.max_header_bytes)
            .serve_connection(TokioIo::new(stream), service);
        let conn = graceful.watch(conn);
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::debug!(peer = %peer, error = %e, "connection ended with an error");
            }
        });
    }
    drop(listener);
    if tokio::time::timeout(limits.drain, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!("shutdown drain timed out; closing remaining connections");
    }
}
