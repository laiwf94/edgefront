//! cachet's request path: the checks a node makes before caching, the
//! origin pool as the upstream, and [`CacheLayer`] for everything the
//! cache does, through the same filter phases the proxy runs it in.
//!
//! The node answers a few requests itself before the cache sees them: a
//! request that already passed through this node (`508`), a target over
//! `max_uri_bytes` (`414`), a request without a host (`400`), and a body
//! over `max_request_body` on a request that passes through (`413`).
//! Everything else goes through the layer on one route, with the
//! `[cache]` settings as its policy. Requests the layer sends upstream get
//! hop-by-hop headers removed, `Via`, `X-Forwarded-For` and the request ID,
//! and go to the origin pool; an origin that fails is reported to the
//! layer by name (`connect`, `timeout`, `protocol`, `body-too-large`), so
//! `stale-if-error` and `Cache-Status` work as for the proxy's errors.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::Bytes;
use edgefront_core::filter::{self, Delivery};
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::uri::PathAndQuery;
use http::{Method, Request, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Limited};
use hyper::body::{Body as HttpBody, Incoming};

use crate::cache_control::CacheControl;
use crate::cluster::{GossipEvent, PurgeHost};
use crate::config::{RoutePolicies, RoutePolicy};
use crate::headers::strip_hop_by_hop;
use crate::ids::{REQUEST_ID, request_id};
use crate::layer::{CACHE_STATUS, CacheLayer, LayerSettings, Outcome};
use crate::metrics::Metrics;
use crate::object::primary_key;
use crate::origin::{OriginError, OriginPool, Outbound};
use crate::purge::PurgeTarget;
use crate::respond::{Body, BoxError, text};
use crate::store::Cache;

pub use crate::layer::{Clock, SystemClock};

#[derive(Debug, Clone)]
pub struct ProxySettings {
    /// Names this node in Via and Cache-Status.
    pub node_id: String,
    pub max_uri_bytes: usize,
    pub max_request_body: u64,
    /// Limit on reading an origin body that is being cached.
    pub body_timeout: Duration,
    /// Cap on heuristic freshness.
    pub heuristic_max: Duration,
}

impl Default for ProxySettings {
    fn default() -> Self {
        ProxySettings {
            node_id: "cachet".into(),
            max_uri_bytes: 8 * 1024,
            max_request_body: 10 << 20,
            body_timeout: Duration::from_secs(30),
            heuristic_max: Duration::from_secs(86_400),
        }
    }
}

pub struct Proxy {
    pub cache: Arc<Cache>,
    pub pool: Arc<OriginPool>,
    pub metrics: Arc<Metrics>,
    layer: Arc<CacheLayer>,
    settings: ProxySettings,
    via: HeaderValue,
    cache_name: String,
}

fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(crate::cache_control::is_tchar)
}

impl Proxy {
    pub fn new(
        cache: Cache,
        pool: Arc<OriginPool>,
        settings: ProxySettings,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        let cache_name = if is_token(&settings.node_id) {
            settings.node_id.clone()
        } else {
            "cachet".to_string()
        };
        let via = HeaderValue::from_str(&format!("1.1 {cache_name}"))
            .expect("token is a valid header value");
        let cache = Arc::new(cache);
        let metrics = Arc::new(Metrics::default());
        // One route, every request on it, with the store's own limits:
        // `store_limited` holds variants to the store's maximum.
        let policy = RoutePolicy {
            max_object_bytes: cache.memory().max_object_bytes() as u64,
            max_variants: usize::MAX,
            heuristic_max: settings.heuristic_max,
            max_ttl: None,
        };
        let layer = CacheLayer::with_observer(
            cache.clone(),
            RoutePolicies::new(vec![Some(policy)]),
            LayerSettings {
                name: cache_name.clone(),
                body_timeout: settings.body_timeout,
                via: Some(via.clone()),
            },
            clock,
            metrics.clone(),
        );
        Arc::new(Proxy {
            cache,
            pool,
            metrics,
            layer,
            settings,
            via,
            cache_name,
        })
    }

    /// Origin fetches currently shared through collapsing, background
    /// refreshes included.
    pub fn in_flight(&self) -> usize {
        self.layer.in_flight()
    }

    /// Answers one client request.
    pub async fn handle<B>(self: &Arc<Self>, req: Request<B>, client: SocketAddr) -> Response<Body>
    where
        B: HttpBody<Data = Bytes> + Send + Sync + 'static,
        B::Error: Into<BoxError>,
    {
        let started = Instant::now();
        let id = request_id(req.headers());
        let method = req.method().clone();
        let uri = req.uri().clone();
        let req = req.map(|b| b.map_err(Into::into).boxed());
        let mut response = self.dispatch(req, &id, client).await;
        response.headers_mut().insert(REQUEST_ID, id.clone());
        let outcome = response
            .extensions()
            .get::<Outcome>()
            .copied()
            .unwrap_or(Outcome::Error);
        let elapsed = started.elapsed();
        self.metrics.request(outcome, elapsed);
        let cache_status = response
            .headers()
            .get(CACHE_STATUS)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix(self.cache_name.as_str()))
            .and_then(|v| v.strip_prefix("; "))
            .unwrap_or("");
        tracing::info!(
            request_id = id.to_str().unwrap_or(""),
            client = %client,
            method = %method,
            uri = %uri,
            status = response.status().as_u16(),
            outcome = outcome.as_str(),
            cache_status = %cache_status,
            elapsed_us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
            "request"
        );
        response
    }

    /// One of the node's own answers.
    fn error(&self, status: StatusCode, message: &'static str, detail: &str) -> Response<Body> {
        let mut resp = text(status, message);
        if let Ok(v) = HeaderValue::from_str(&format!("{}; {detail}", self.cache_name)) {
            resp.headers_mut().insert(CACHE_STATUS, v);
        }
        resp.extensions_mut().insert(Outcome::Error);
        resp
    }

    async fn dispatch(
        self: &Arc<Self>,
        req: Request<Body>,
        id: &HeaderValue,
        client: SocketAddr,
    ) -> Response<Body> {
        if self.looped(req.headers()) {
            return self.error(
                StatusCode::LOOP_DETECTED,
                "request loop detected\n",
                "detail=loop",
            );
        }
        if req.uri().to_string().len() > self.settings.max_uri_bytes {
            return self.error(
                StatusCode::URI_TOO_LONG,
                "request target too long\n",
                "detail=uri-too-long",
            );
        }
        if primary_key(req.uri(), req.headers()).is_none() {
            return self.error(StatusCode::BAD_REQUEST, "missing host\n", "detail=no-host");
        }
        let passes = !matches!(*req.method(), Method::GET | Method::HEAD)
            || CacheControl::from_headers(req.headers()).no_store;
        if passes
            && declared_length(req.headers()).is_some_and(|n| n > self.settings.max_request_body)
        {
            return self.error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large\n",
                "detail=body-too-large",
            );
        }
        let proxy = self.clone();
        let (id, client) = (id.clone(), client);
        filter::run(Some(&*self.layer), 0, req, move |req| {
            let proxy = proxy.clone();
            let id = id.clone();
            async move { proxy.send(req, &id, client).await }
        })
        .await
    }

    fn looped(&self, headers: &HeaderMap) -> bool {
        headers.get_all(header::VIA).iter().any(|v| {
            v.to_str().is_ok_and(|s| {
                s.split(',')
                    .any(|hop| hop.split_whitespace().nth(1) == Some(&self.cache_name))
            })
        })
    }

    /// Sends a request the cache layer forwards to the origin pool.
    async fn send(
        &self,
        req: Request<Body>,
        id: &HeaderValue,
        client: SocketAddr,
    ) -> (Response<Body>, Delivery) {
        let (parts, body) = req.into_parts();
        let declared = declared_length(&parts.headers);
        let has_body = !(parts.method == Method::GET || parts.method == Method::HEAD)
            || declared.is_some_and(|n| n > 0)
            || parts.headers.contains_key(header::TRANSFER_ENCODING);
        let limit = usize::try_from(self.settings.max_request_body).unwrap_or(usize::MAX);
        let out = Outbound {
            method: parts.method,
            path: path_of(&parts.uri),
            headers: self.outbound_headers(parts.headers, id, client),
            body: has_body.then(|| Limited::new(body, limit).boxed()),
        };
        match self.pool.send(out).await {
            Ok((resp, _origin)) => {
                let (mut parts, body) = resp.into_parts();
                strip_hop_by_hop(&mut parts.headers);
                let body = body.map_err(Into::into).boxed();
                (Response::from_parts(parts, body), Delivery::Upstream)
            }
            Err(e) => {
                let (status, message) = match e {
                    OriginError::Connect => (StatusCode::BAD_GATEWAY, "origin unreachable\n"),
                    OriginError::Timeout => (StatusCode::GATEWAY_TIMEOUT, "origin timed out\n"),
                    OriginError::Protocol => (StatusCode::BAD_GATEWAY, "origin response invalid\n"),
                    OriginError::RequestBody => {
                        (StatusCode::PAYLOAD_TOO_LARGE, "request body too large\n")
                    }
                };
                (text(status, message), Delivery::Generated(e.as_str()))
            }
        }
    }

    /// Headers for a request to the origin.
    fn outbound_headers(
        &self,
        mut out: HeaderMap,
        id: &HeaderValue,
        client: SocketAddr,
    ) -> HeaderMap {
        strip_hop_by_hop(&mut out);
        out.append(header::VIA, self.via.clone());
        let ip = client.ip().to_string();
        let forwarded = match out.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            Some(prev) => format!("{prev}, {ip}"),
            None => ip,
        };
        if let Ok(v) = HeaderValue::from_str(&forwarded) {
            out.insert(HeaderName::from_static("x-forwarded-for"), v);
        }
        out.insert(REQUEST_ID, id.clone());
        out
    }
}

fn declared_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
}

fn path_of(uri: &Uri) -> PathAndQuery {
    uri.path_and_query()
        .cloned()
        .unwrap_or_else(|| PathAndQuery::from_static("/"))
}

impl crate::listener::Handler for Proxy {
    type Body = Body;

    async fn call(self: Arc<Self>, req: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
        self.handle(req, peer).await
    }
}

impl PurgeHost for Proxy {
    fn purge<'a>(
        &'a self,
        targets: &'a [PurgeTarget],
    ) -> Pin<Box<dyn Future<Output = usize> + Send + 'a>> {
        Box::pin(self.cache.purge(targets))
    }

    fn record(&self, event: GossipEvent) {
        let m = &self.metrics;
        match event {
            GossipEvent::Sent => m.gossip_sent.fetch_add(1, Ordering::Relaxed),
            GossipEvent::Retried => m.gossip_retries.fetch_add(1, Ordering::Relaxed),
            GossipEvent::Dropped => m.gossip_dropped.fetch_add(1, Ordering::Relaxed),
            GossipEvent::Rejected => m.gossip_rejected.fetch_add(1, Ordering::Relaxed),
            GossipEvent::Applied { removed } => {
                m.purges_peer.fetch_add(1, Ordering::Relaxed);
                m.purged_objects
                    .fetch_add(removed as u64, Ordering::Relaxed)
            }
        };
    }
}
