//! The HTTP version a cluster speaks to its endpoints: its `protocol`
//! setting, and the downgrade from HTTP/2 to HTTP/1.1 under `auto`.
//!
//! | `protocol` | cleartext endpoint | TLS endpoint |
//! |---|---|---|
//! | `http1` (default) | HTTP/1.1 | HTTP/1.1, `http/1.1` over ALPN |
//! | `http2` | HTTP/2 with prior knowledge | HTTP/2, `h2` over ALPN |
//! | `auto` | prior knowledge first, HTTP/1.1 if refused | `h2` and `http/1.1` over ALPN, as negotiated |
//!
//! HTTP/2 goes through the cluster's [`Pool`]: streams multiplexed on a
//! few connections per endpoint. Under `http2`, an endpoint that does not
//! speak HTTP/2 fails the attempt like a refused connection. Under `auto`,
//! such an endpoint is downgraded: it is sent this request and every
//! request for [`DOWNGRADE_FOR`] over HTTP/1.1, then HTTP/2 is tried
//! again. The question is settled before the request is sent (the TLS
//! ALPN answer, or whether a prior-knowledge origin's first frame is
//! `SETTINGS`), so a downgrade never sends a request twice.
//!
//! Health checks speak HTTP/1.1 whatever the protocol.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use edgefront_core::body::{Body, BoxError};
use edgefront_core::config::UpstreamProtocol;
use edgefront_core::h2::{Pool, PoolBody, PoolError, PoolLimits, Settings};
use edgefront_core::http1::{Client, ClientError, ClientLimits, TlsTo};
use http::{Request, Response};
use hyper::body::{Body as HttpBody, Frame, Incoming, SizeHint};

/// How long an endpoint that refused HTTP/2 under `auto` is sent
/// HTTP/1.1 before HTTP/2 is tried again.
pub const DOWNGRADE_FOR: Duration = Duration::from_secs(300);

/// An upstream response body, from either protocol.
pub enum ResponseBody {
    Http1(Incoming),
    Http2(PoolBody),
}

impl HttpBody for ResponseBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        match self.get_mut() {
            ResponseBody::Http1(b) => Pin::new(b)
                .poll_frame(cx)
                .map(|f| f.map(|r| r.map_err(BoxError::from))),
            ResponseBody::Http2(b) => Pin::new(b).poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            ResponseBody::Http1(b) => b.is_end_stream(),
            ResponseBody::Http2(b) => b.is_end_stream(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            ResponseBody::Http1(b) => b.size_hint(),
            ResponseBody::Http2(b) => b.size_hint(),
        }
    }
}

/// A cluster's way to its endpoints.
pub struct Upstream {
    protocol: UpstreamProtocol,
    http1: Client<Body>,
    http2: Option<Pool<Body>>,
    /// Under `auto`, endpoints that refused HTTP/2, until when.
    downgraded: Mutex<HashMap<String, Instant>>,
}

/// How a request was answered, for metrics and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spoken {
    Http1,
    Http2,
}

impl Upstream {
    /// The way to a cluster's endpoints for `protocol`, over TLS when
    /// `tls` is set. Must be called inside a tokio runtime.
    pub fn new(protocol: UpstreamProtocol, limits: &ClientLimits, tls: Option<&TlsTo>) -> Upstream {
        let http1 = match tls {
            Some(t) => Client::with_tls(limits, t.clone()),
            None => Client::new(limits),
        };
        Upstream::with_http1(protocol, limits, tls, http1)
    }

    /// Like [`new`](Self::new), keeping an existing HTTP/1.1 client and
    /// its pool of connections.
    pub fn with_http1(
        protocol: UpstreamProtocol,
        limits: &ClientLimits,
        tls: Option<&TlsTo>,
        http1: Client<Body>,
    ) -> Upstream {
        let pool_limits = PoolLimits {
            connect_timeout: limits.connect_timeout,
            idle_timeout: limits.pool_idle_timeout,
            ..PoolLimits::default()
        };
        // Origins sit at any distance, so upstream windows follow the
        // measured bandwidth-delay product.
        let settings = Settings {
            adaptive_window: true,
            ..Settings::default()
        };
        let http2 = match (protocol, tls) {
            (UpstreamProtocol::Http1, _) => None,
            (UpstreamProtocol::Http2, None) | (UpstreamProtocol::Auto, None) => {
                Some(Pool::new(pool_limits, settings))
            }
            (UpstreamProtocol::Http2, Some(t)) => {
                Some(Pool::with_tls(pool_limits, settings, t.clone()))
            }
            (UpstreamProtocol::Auto, Some(t)) => {
                Some(Pool::with_tls_auto(pool_limits, settings, t.clone()))
            }
        };
        Upstream {
            protocol,
            http1,
            http2,
            downgraded: Mutex::new(HashMap::new()),
        }
    }

    /// A way that fails every request, for a cluster whose TLS settings
    /// could not be read.
    pub fn refusing(limits: &ClientLimits) -> Upstream {
        Upstream {
            protocol: UpstreamProtocol::Http1,
            http1: Client::refusing(limits),
            http2: None,
            downgraded: Mutex::new(HashMap::new()),
        }
    }

    /// The HTTP/1.1 client, which a reload can carry over.
    pub fn http1(&self) -> &Client<Body> {
        &self.http1
    }

    /// The configured protocol.
    pub fn protocol(&self) -> UpstreamProtocol {
        self.protocol
    }

    fn downgraded(&self, authority: &str) -> bool {
        let mut map = self.downgraded.lock().unwrap_or_else(|p| p.into_inner());
        match map.get(authority) {
            Some(until) if *until > Instant::now() => true,
            Some(_) => {
                map.remove(authority);
                false
            }
            None => false,
        }
    }

    fn downgrade(&self, authority: &str) {
        tracing::info!(
            endpoint = authority,
            for_secs = DOWNGRADE_FOR.as_secs(),
            "endpoint does not speak http/2; using http/1.1"
        );
        self.downgraded
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(authority.to_owned(), Instant::now() + DOWNGRADE_FOR);
    }

    /// Sends `req` (an absolute `http://host:port/path` URI naming the
    /// endpoint) and returns once the response head has arrived, with the
    /// protocol it went over.
    pub async fn send(
        &self,
        req: Request<Body>,
    ) -> Result<(Response<ResponseBody>, Spoken), ClientError> {
        let Some(pool) = &self.http2 else {
            return self.send_http1(req).await;
        };
        if self.protocol == UpstreamProtocol::Auto {
            let authority = req
                .uri()
                .authority()
                .map(|a| a.as_str().to_owned())
                .unwrap_or_default();
            if self.downgraded(&authority) {
                return self.send_http1(req).await;
            }
            match pool.prepare(req.uri()).await {
                Ok(()) => {}
                Err(PoolError::NotHttp2) => {
                    self.downgrade(&authority);
                    return self.send_http1(req).await;
                }
                Err(e) => return Err(client_error(e)),
            }
        }
        match pool.send(req).await {
            Ok(resp) => Ok((resp.map(ResponseBody::Http2), Spoken::Http2)),
            Err(e) => Err(client_error(e)),
        }
    }

    async fn send_http1(
        &self,
        req: Request<Body>,
    ) -> Result<(Response<ResponseBody>, Spoken), ClientError> {
        let resp = self.http1.send(req).await?;
        Ok((resp.map(ResponseBody::Http1), Spoken::Http1))
    }
}

/// The HTTP/1.1 client's error for a pool's, as the retry policy reads it:
/// a stream the origin refused unprocessed, or an endpoint without
/// HTTP/2 under `http2`, is a connect error, which is always safe to try
/// elsewhere.
pub fn client_error(e: PoolError) -> ClientError {
    match e {
        PoolError::Connect | PoolError::NotHttp2 | PoolError::Refused => ClientError::Connect,
        PoolError::ConnectTimeout => ClientError::ConnectTimeout,
        PoolError::Protocol => ClientError::Protocol,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_errors_map_to_retryable_client_errors() {
        assert_eq!(client_error(PoolError::Refused), ClientError::Connect);
        assert_eq!(client_error(PoolError::NotHttp2), ClientError::Connect);
        assert_eq!(client_error(PoolError::Connect), ClientError::Connect);
        assert_eq!(
            client_error(PoolError::ConnectTimeout),
            ClientError::ConnectTimeout
        );
        assert_eq!(client_error(PoolError::Protocol), ClientError::Protocol);
    }

    #[tokio::test]
    async fn downgrades_expire() {
        let up = Upstream::new(UpstreamProtocol::Auto, &ClientLimits::default(), None);
        assert!(!up.downgraded("a:1"));
        up.downgrade("a:1");
        assert!(up.downgraded("a:1"));
        up.downgraded
            .lock()
            .unwrap()
            .insert("a:1".into(), Instant::now() - Duration::from_secs(1));
        assert!(!up.downgraded("a:1"));
        assert!(up.downgraded.lock().unwrap().is_empty());
        assert_eq!(up.protocol(), UpstreamProtocol::Auto);
    }
}
