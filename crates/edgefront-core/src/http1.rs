//! HTTP/1.1 on hyper: the server connection settings the listener uses,
//! and the pooled client that proxies and caches use to reach origins.
//!
//! Every limit has a default taken from steer, so the Rust port accepts and
//! refuses the same traffic: request heads up to 64 KiB, origin connects
//! that give up after one second, and idle origin connections kept for 90
//! seconds, at most 256 per origin.

use std::error::Error as StdError;
use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use http::{Request, Response};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::server::conn::http1;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioTimer};

/// The smallest read buffer hyper accepts. A configured header limit below
/// it is still enforced exactly on the server, through hyper's separate
/// header size check; on the client the buffer is the limit, so response
/// heads up to this size are always accepted.
pub const MIN_BUFFER: usize = 8192;

/// The boxed error type hyper accepts from response bodies.
pub type BoxError = Box<dyn StdError + Send + Sync>;

/// Builds the server side of an HTTP/1.1 connection with the head limits.
///
/// The header read timeout is left to the listener: hyper starts that
/// timer as soon as it waits for a request, so it would also close idle
/// keep-alive connections, while the listener starts it at the first byte
/// of a request head and gives idle connections their own timeout.
pub(crate) fn server_builder(max_header_bytes: usize, max_headers: usize) -> http1::Builder {
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(None)
        .keep_alive(true)
        .max_buf_size(max_header_bytes.max(MIN_BUFFER))
        .max_header_size(max_header_bytes)
        .max_headers(max_headers);
    builder
}

/// A response body that holds `G` until hyper drops the body, which it does
/// once the last byte is written or the connection dies. The listener uses
/// it to count a request as in flight until its response is fully sent.
pub(crate) struct HoldBody<B, G> {
    body: B,
    _guard: G,
}

impl<B, G> HoldBody<B, G> {
    pub(crate) fn new(body: B, guard: G) -> Self {
        HoldBody {
            body,
            _guard: guard,
        }
    }
}

impl<B: Body + Unpin, G: Unpin> Body for HoldBody<B, G> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.get_mut().body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// Limits for the pooled client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientLimits {
    /// How long a TCP connect may take.
    pub connect_timeout: Duration,
    /// How long to wait for the response head once the request is handed
    /// to the pool, connect included. `None` waits as long as the origin
    /// takes, which streaming and long-poll routes need.
    pub response_header_timeout: Option<Duration>,
    /// How long an idle pooled connection is kept before it is closed.
    pub pool_idle_timeout: Duration,
    /// How many idle connections are kept per origin. Zero turns pooling
    /// off: every request opens its own connection.
    pub max_idle_per_host: usize,
    /// Largest response head (status line and headers) accepted. Values
    /// below [`MIN_BUFFER`] behave as [`MIN_BUFFER`].
    pub max_response_header_bytes: usize,
}

impl Default for ClientLimits {
    /// steer's origin transport: a one-second connect, 90 seconds of idle
    /// keep-alive, 256 idle connections per origin and a 15-second wait for
    /// the response, which steer applies per cluster.
    fn default() -> Self {
        ClientLimits {
            connect_timeout: Duration::from_secs(1),
            response_header_timeout: Some(Duration::from_secs(15)),
            pool_idle_timeout: Duration::from_secs(90),
            max_idle_per_host: 256,
            max_response_header_bytes: 64 << 10,
        }
    }
}

/// Why a request through the client failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientError {
    /// The connection was refused, reset, or could not be routed.
    Connect,
    /// The TCP connect took longer than `connect_timeout`.
    ConnectTimeout,
    /// No response head within `response_header_timeout`.
    Timeout,
    /// The response head was larger than `max_response_header_bytes`.
    HeadersTooLarge,
    /// The connection broke or the origin sent something that is not
    /// HTTP/1.1.
    Protocol,
}

impl ClientError {
    /// A short stable name for logs and metric labels.
    pub fn as_str(self) -> &'static str {
        match self {
            ClientError::Connect => "connect",
            ClientError::ConnectTimeout => "connect_timeout",
            ClientError::Timeout => "timeout",
            ClientError::HeadersTooLarge => "headers_too_large",
            ClientError::Protocol => "protocol",
        }
    }

    fn classify(e: &hyper_util::client::legacy::Error) -> ClientError {
        let mut source = e.source();
        while let Some(s) = source {
            if let Some(io) = s.downcast_ref::<std::io::Error>()
                && io.kind() == std::io::ErrorKind::TimedOut
                && e.is_connect()
            {
                return ClientError::ConnectTimeout;
            }
            if let Some(h) = s.downcast_ref::<hyper::Error>()
                && h.is_parse_too_large()
            {
                return ClientError::HeadersTooLarge;
            }
            source = s.source();
        }
        if e.is_connect() {
            ClientError::Connect
        } else {
            ClientError::Protocol
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl StdError for ClientError {}

/// A pooled HTTP/1.1 client over plain TCP. Cloning is cheap and clones
/// share one pool.
pub struct Client<B> {
    inner: HyperClient<HttpConnector, B>,
    response_header_timeout: Option<Duration>,
}

impl<B> Clone for Client<B> {
    fn clone(&self) -> Self {
        Client {
            inner: self.inner.clone(),
            response_header_timeout: self.response_header_timeout,
        }
    }
}

impl<B> fmt::Debug for Client<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("response_header_timeout", &self.response_header_timeout)
            .finish_non_exhaustive()
    }
}

impl<B> Client<B>
where
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    /// Builds a client with its own pool. Must be called inside a tokio
    /// runtime, which runs the pool's idle reaper.
    pub fn new(limits: &ClientLimits) -> Self {
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(limits.connect_timeout));
        connector.set_nodelay(true);
        connector.enforce_http(true);
        let inner = HyperClient::builder(TokioExecutor::new())
            .timer(TokioTimer::new())
            .pool_timer(TokioTimer::new())
            .pool_idle_timeout(limits.pool_idle_timeout)
            .pool_max_idle_per_host(limits.max_idle_per_host)
            .http1_max_buf_size(limits.max_response_header_bytes.max(MIN_BUFFER))
            .build(connector);
        Client {
            inner,
            response_header_timeout: limits.response_header_timeout,
        }
    }

    /// Sends `req`, whose URI must be absolute (`http://host:port/path`),
    /// and returns once the response head has arrived. The body streams
    /// afterwards; the connection goes back to the pool when it is read to
    /// the end.
    pub async fn send(&self, req: Request<B>) -> Result<Response<Incoming>, ClientError> {
        let sent = self.inner.request(req);
        let result = match self.response_header_timeout {
            Some(limit) => match tokio::time::timeout(limit, sent).await {
                Ok(result) => result,
                Err(_) => return Err(ClientError::Timeout),
            },
            None => sent.await,
        };
        result.map_err(|e| ClientError::classify(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_names_are_stable() {
        let all = [
            (ClientError::Connect, "connect"),
            (ClientError::ConnectTimeout, "connect_timeout"),
            (ClientError::Timeout, "timeout"),
            (ClientError::HeadersTooLarge, "headers_too_large"),
            (ClientError::Protocol, "protocol"),
        ];
        for (e, name) in all {
            assert_eq!(e.as_str(), name);
            assert_eq!(e.to_string(), name);
        }
    }

    #[test]
    fn defaults_match_steer() {
        let l = ClientLimits::default();
        assert_eq!(l.connect_timeout, Duration::from_secs(1));
        assert_eq!(l.pool_idle_timeout, Duration::from_secs(90));
        assert_eq!(l.max_idle_per_host, 256);
        assert_eq!(l.max_response_header_bytes, 65536);
    }

    #[test]
    fn small_header_limits_still_build() {
        // hyper panics on read buffers under 8 KiB; the builders clamp.
        let _ = server_builder(16, 4);
    }
}
