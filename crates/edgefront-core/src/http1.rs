//! HTTP/1.1 on hyper: the server connection settings the listener uses,
//! and the pooled client that proxies and caches use to reach origins.
//!
//! Every limit has a default taken from steer, so the Rust port accepts and
//! refuses the same traffic: request heads up to 64 KiB, origin connects
//! that give up after one second, and idle origin connections kept for 90
//! seconds, at most 256 per origin.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::{Request, Response};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::client::conn::http1 as conn_http1;
use hyper::server::conn::http1;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;

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

/// A pooled HTTP/1.1 client, over plain TCP or, with
/// [`Client::with_tls`], over TLS to every origin. Cloning is cheap and
/// clones share one pool.
pub struct Client<B> {
    inner: HyperClient<HttpConnector, B>,
    response_header_timeout: Option<Duration>,
    tls: Option<Arc<TlsPool<B>>>,
    refuse: bool,
}

impl<B> Clone for Client<B> {
    fn clone(&self) -> Self {
        Client {
            inner: self.inner.clone(),
            response_header_timeout: self.response_header_timeout,
            tls: self.tls.clone(),
            refuse: self.refuse,
        }
    }
}

/// How a client speaks TLS to its origins: the trust anchors and, for
/// mutual TLS, the client certificate (a `ClientConfig`), and the name
/// the origins' certificates must carry when it is not the host in the
/// request's URI.
#[derive(Clone)]
pub struct TlsTo {
    config: Arc<rustls::ClientConfig>,
    server_name: Option<ServerName<'static>>,
}

impl fmt::Debug for TlsTo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsTo")
            .field("server_name", &self.server_name)
            .finish_non_exhaustive()
    }
}

impl TlsTo {
    /// Reads a cluster's `tls` block: the CA bundle it pins (the only
    /// trust anchors; the system's are never used), the server name
    /// override, and the client certificate and key for mutual TLS.
    pub fn load(cfg: &crate::config::UpstreamTls) -> Result<TlsTo, String> {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let file = &cfg.ca_file;
        let mut roots = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_file_iter(file).map_err(|e| format!("{file}: {e}"))? {
            roots
                .add(cert.map_err(|e| format!("{file}: {e}"))?)
                .map_err(|e| format!("{file}: {e}"))?;
        }
        if roots.is_empty() {
            return Err(format!("{file}: no CA certificates"));
        }
        let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots);
        let mut config = match (&cfg.cert_file, &cfg.key_file) {
            (Some(c), Some(k)) => {
                let chain = CertificateDer::pem_file_iter(c)
                    .map_err(|e| format!("{c}: {e}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("{c}: {e}"))?;
                let key = PrivateKeyDer::from_pem_file(k).map_err(|e| format!("{k}: {e}"))?;
                builder
                    .with_client_auth_cert(chain, key)
                    .map_err(|e| format!("{c} and {k}: {e}"))?
            }
            _ => builder.with_no_client_auth(),
        };
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let server_name = match &cfg.server_name {
            Some(n) => Some(
                ServerName::try_from(n.clone()).map_err(|e| format!("server_name {n:?}: {e}"))?,
            ),
            None => None,
        };
        Ok(TlsTo {
            config: Arc::new(config),
            server_name,
        })
    }
}

/// An idle pooled connection and when it went idle.
type Idle<B> = (conn_http1::SendRequest<B>, Instant);

/// Idle TLS connections by authority.
struct TlsPool<B> {
    tls: TlsTo,
    connect_timeout: Duration,
    idle_timeout: Duration,
    max_idle: usize,
    max_buf: usize,
    idle: Mutex<HashMap<String, Vec<Idle<B>>>>,
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
            tls: None,
            refuse: false,
        }
    }

    /// A client every request through which fails with
    /// [`ClientError::Connect`], for a cluster whose TLS settings could not
    /// be read: it must not fall back to cleartext.
    pub fn refusing(limits: &ClientLimits) -> Self {
        let mut client = Client::new(limits);
        client.refuse = true;
        client
    }

    /// A client that speaks TLS to every origin, as `tls` says, with the
    /// same limits and its own pool of TLS connections. Requests still
    /// carry `http://host:port/path` URIs, whose authority names the
    /// origin to connect to.
    pub fn with_tls(limits: &ClientLimits, tls: TlsTo) -> Self {
        let mut client = Client::new(limits);
        client.tls = Some(Arc::new(TlsPool {
            tls,
            connect_timeout: limits.connect_timeout,
            idle_timeout: limits.pool_idle_timeout,
            max_idle: limits.max_idle_per_host,
            max_buf: limits.max_response_header_bytes.max(MIN_BUFFER),
            idle: Mutex::new(HashMap::new()),
        }));
        client
    }

    /// Sends `req`, whose URI must be absolute (`http://host:port/path`),
    /// and returns once the response head has arrived. The body streams
    /// afterwards; the connection goes back to the pool when it is read to
    /// the end.
    pub async fn send(&self, req: Request<B>) -> Result<Response<Incoming>, ClientError> {
        if self.refuse {
            return Err(ClientError::Connect);
        }
        if let Some(pool) = &self.tls {
            let sent = pool.send(req);
            return match self.response_header_timeout {
                Some(limit) => tokio::time::timeout(limit, sent)
                    .await
                    .unwrap_or(Err(ClientError::Timeout)),
                None => sent.await,
            };
        }
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

impl<B> TlsPool<B>
where
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    /// An idle connection to `authority` that can take a request now.
    fn take_idle(&self, authority: &str) -> Option<conn_http1::SendRequest<B>> {
        let mut idle = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        let list = idle.get_mut(authority)?;
        let mut busy = Vec::new();
        let mut found = None;
        while let Some((sender, since)) = list.pop() {
            if sender.is_closed() || since.elapsed() >= self.idle_timeout {
                continue;
            }
            if sender.is_ready() {
                found = Some(sender);
                break;
            }
            // Still reading the last response's body; it comes back later.
            busy.push((sender, since));
        }
        list.extend(busy);
        found
    }

    fn put_idle(&self, authority: &str, sender: conn_http1::SendRequest<B>) {
        if self.max_idle == 0 {
            return;
        }
        let mut idle = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        let list = idle.entry(authority.to_owned()).or_default();
        list.retain(|(s, since)| !s.is_closed() && since.elapsed() < self.idle_timeout);
        if list.len() < self.max_idle {
            list.push((sender, Instant::now()));
        }
    }

    async fn connect(
        &self,
        authority: &str,
        host: &str,
    ) -> Result<conn_http1::SendRequest<B>, ClientError> {
        let tcp = tokio::time::timeout(self.connect_timeout, TcpStream::connect(authority))
            .await
            .map_err(|_| ClientError::ConnectTimeout)?
            .map_err(|_| ClientError::Connect)?;
        let _ = tcp.set_nodelay(true);
        let name = match &self.tls.server_name {
            Some(n) => n.clone(),
            None => ServerName::try_from(host.trim_matches(['[', ']']).to_owned())
                .map_err(|_| ClientError::Connect)?,
        };
        let connector = tokio_rustls::TlsConnector::from(Arc::clone(&self.tls.config));
        let stream = tokio::time::timeout(self.connect_timeout, connector.connect(name, tcp))
            .await
            .map_err(|_| ClientError::ConnectTimeout)?
            .map_err(|e| {
                tracing::debug!(origin = authority, error = %e, "tls to the origin failed");
                ClientError::Connect
            })?;
        let (sender, conn) = conn_http1::Builder::new()
            .max_buf_size(self.max_buf)
            .handshake(TokioIo::new(stream))
            .await
            .map_err(|_| ClientError::Connect)?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(sender)
    }

    async fn send(&self, mut req: Request<B>) -> Result<Response<Incoming>, ClientError> {
        let authority = req
            .uri()
            .authority()
            .map(|a| a.as_str().to_owned())
            .ok_or(ClientError::Protocol)?;
        let host = req.uri().host().unwrap_or_default().to_owned();
        let authority = match req.uri().port_u16() {
            Some(_) => authority,
            None => format!("{authority}:443"),
        };
        // The connection is to the origin, so the request line is the
        // origin form.
        let origin_form = req.uri().path_and_query().map_or_else(
            || http::Uri::from_static("/"),
            |p| http::Uri::from(p.clone()),
        );
        *req.uri_mut() = origin_form;
        let mut sender = match self.take_idle(&authority) {
            Some(s) => s,
            None => self.connect(&authority, &host).await?,
        };
        match sender.send_request(req).await {
            Ok(resp) => {
                self.put_idle(&authority, sender);
                Ok(resp)
            }
            Err(e) if e.is_parse_too_large() => Err(ClientError::HeadersTooLarge),
            Err(_) => Err(ClientError::Protocol),
        }
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
