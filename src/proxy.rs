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
//!
//! With `[tier]`, a miss goes to its owner among the edge nodes, or the
//! route's shield, before the origin ([`edgefront_cache::tier`]): a node
//! that does not own a key passes the owner's response on without storing
//! it, unless the key is hot here, and goes to the origin itself when the
//! owner cannot be reached.

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
use crate::origin::{OriginError, OriginPool, OutBody, Outbound};

use crate::purge::PurgeTarget;
use crate::respond::{Body, BoxError, text};
use crate::store::Cache;
use edgefront_cache::tier::peer::{self, HOP};
use edgefront_cache::tier::{Next, Tier, Transient};

pub use crate::layer::{Clock, SystemClock};

/// The largest request body (by Content-Length) held whole so that an
/// idempotent request can be sent again.
const REPLAY_BODY_MAX: u64 = 64 * 1024;

/// Reads a request body of at most [`REPLAY_BODY_MAX`] bytes whole.
async fn read_replayable(body: Body) -> Result<Bytes, ()> {
    let mut body = Limited::new(body, REPLAY_BODY_MAX as usize);
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.map_err(|_| ())?.into_data() {
            out.extend_from_slice(&data);
        }
    }
    Ok(Bytes::from(out))
}

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
    /// Slice size for responses over the object limit; 0 to stream them
    /// through uncached.
    pub slice_bytes: u64,
    /// Whether text-like responses are stored with gzip, brotli and zstd
    /// variants, and how ([`edgefront_cache::variants`]).
    pub compress: bool,
    pub compression: edgefront_cache::variants::CompressionSettings,
}

impl Default for ProxySettings {
    fn default() -> Self {
        ProxySettings {
            node_id: "cachet".into(),
            max_uri_bytes: 8 * 1024,
            max_request_body: 10 << 20,
            body_timeout: Duration::from_secs(30),
            heuristic_max: Duration::from_secs(86_400),
            slice_bytes: edgefront_cache::slice::DEFAULT_SLICE_BYTES,
            compress: false,
            compression: Default::default(),
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
    tier: std::sync::OnceLock<TierLink>,
}

/// A node's link to the tiers: the membership view and hot keys, and the
/// client it fetches from other nodes on.
pub struct TierLink {
    pub tier: Arc<Tier>,
    pub client: crate::origin::Client,
    pub timeout: Duration,
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
            slice_bytes: edgefront_cache::slice::effective(
                settings.slice_bytes,
                cache.memory().max_object_bytes() as u64,
            ),
            compress: settings.compress,
        };
        let layer = CacheLayer::with_observer(
            cache.clone(),
            RoutePolicies::new(vec![Some(policy)]),
            LayerSettings {
                name: cache_name.clone(),
                body_timeout: settings.body_timeout,
                via: Some(via.clone()),
                compression: settings.compression.clone(),
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
            tier: std::sync::OnceLock::new(),
        })
    }

    /// Joins the tiers: misses go to owners and shields from now on.
    /// Returns false if already joined.
    pub fn enable_tier(&self, link: TierLink) -> bool {
        self.tier.set(link).is_ok()
    }

    /// The tiers, when this node is in a cluster.
    pub fn tier(&self) -> Option<&Arc<Tier>> {
        self.tier.get().map(|l| &l.tier)
    }

    /// Origin fetches currently shared through collapsing, background
    /// refreshes included.
    pub fn in_flight(&self) -> usize {
        self.layer.in_flight()
    }

    /// The compressor making the gzip, brotli and zstd variants.
    pub fn compressor(&self) -> &Arc<edgefront_cache::variants::Compressor> {
        self.layer.compressor()
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
        let mut req = req;
        if let Some(link) = self.tier.get() {
            self.tier_observe(link, &mut req).await;
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

    /// Before the cache: drops a hop header that is not validly signed, and
    /// counts the request for the hot-key sketch, dropping this node's copies
    /// of keys it demoted that another node owns.
    async fn tier_observe(&self, link: &TierLink, req: &mut Request<Body>) {
        let key_bytes = &link.tier.settings.key;
        if let Some(v) = req.headers().get(HOP) {
            let target = req.uri().path_and_query().map_or("/", |p| p.as_str());
            if peer::verify(key_bytes, v, req.method().as_str(), target).is_none() {
                req.headers_mut().remove(HOP);
            }
        }
        if !matches!(*req.method(), Method::GET | Method::HEAD) {
            return;
        }
        let Some(key) = primary_key(req.uri(), req.headers()) else {
            return;
        };
        let (promoted, demoted) = link.tier.observe(&key);
        if promoted {
            self.push_replicas(link, req, &key);
        }
        let gone: Vec<PurgeTarget> = demoted
            .into_iter()
            .filter(|k| !link.tier.keeps(k))
            .map(|k| PurgeTarget::Key(k.into()))
            .collect();
        if !gone.is_empty() {
            self.cache.purge(&gone).await;
        }
    }

    /// A key this node owns just became hot: its runners-up are told to
    /// fetch it from here and keep it, in the background.
    fn push_replicas(&self, link: &TierLink, req: &Request<Body>, key: &str) {
        let replicas = link.tier.replicas(key);
        if replicas.is_empty() {
            return;
        }
        let target = req
            .uri()
            .path_and_query()
            .map_or("/", |p| p.as_str())
            .to_owned();
        let hop = peer::sign(
            &link.tier.settings.key,
            &link.tier.settings.me,
            peer::Tier::Replica,
            "GET",
            &target,
        );
        let host = req.headers().get(header::HOST).cloned();
        for (_, addr) in replicas {
            let Ok(uri) = format!("http://{addr}{target}").parse::<Uri>() else {
                continue;
            };
            let mut out = Request::new(crate::respond::empty());
            *out.uri_mut() = uri;
            if let Some(h) = &host {
                out.headers_mut().insert(header::HOST, h.clone());
            }
            // No Via: the replica sends the request back here, and this
            // node's name in Via would make that a loop.
            out.headers_mut().insert(HOP, hop.clone());
            let client = link.client.clone();
            let timeout = link.timeout;
            link.tier
                .stats
                .replicas_sent
                .fetch_add(1, Ordering::Relaxed);
            tokio::spawn(async move {
                if let Ok(Ok(resp)) = tokio::time::timeout(timeout, client.request(out)).await {
                    // Read to the end, so the replica's fill completes.
                    let _ = resp.into_body().collect().await;
                }
            });
        }
    }

    /// Sends a miss to another node, if the tiers say so. `None` when it goes
    /// to the origin: by the tiers' choice, or because the node could not be
    /// reached in time.
    async fn send_to_tier(
        &self,
        parts: &http::request::Parts,
        id: &HeaderValue,
        client: SocketAddr,
    ) -> Option<Response<Body>> {
        let link = self.tier.get()?;
        if !matches!(parts.method, Method::GET | Method::HEAD) {
            return None;
        }
        let target = parts.uri.path_and_query().map_or("/", |p| p.as_str());
        let key_bytes = &link.tier.settings.key;
        let hop = parts
            .headers
            .get(HOP)
            .and_then(|v| peer::verify(key_bytes, v, parts.method.as_str(), target));
        let key = primary_key(&parts.uri, &parts.headers)?;
        let host = parts
            .headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let Next::Node { addr, tier, .. } =
            link.tier.next(&key, host, parts.uri.path(), hop.as_ref())
        else {
            return None;
        };
        let stats = &link.tier.stats;
        match tier {
            peer::Tier::Owner => stats.owner_fetches.fetch_add(1, Ordering::Relaxed),
            peer::Tier::Shield => stats.shield_fetches.fetch_add(1, Ordering::Relaxed),
            peer::Tier::Replica => 0,
        };
        let mut headers = self.outbound_headers(parts.headers.clone(), id, client);
        headers.insert(
            HOP,
            peer::sign(
                key_bytes,
                &link.tier.settings.me,
                tier,
                parts.method.as_str(),
                target,
            ),
        );
        let uri: Uri = format!("http://{addr}{target}").parse().ok()?;
        let mut req = Request::new(crate::respond::empty());
        *req.method_mut() = parts.method.clone();
        *req.uri_mut() = uri;
        *req.headers_mut() = headers;
        match tokio::time::timeout(link.timeout, link.client.request(req)).await {
            Ok(Ok(resp)) => {
                let (mut parts, body) = resp.into_parts();
                strip_hop_by_hop(&mut parts.headers);
                parts.headers.remove(HOP);
                let replica = hop.as_ref().is_some_and(|h| h.tier == peer::Tier::Replica);
                if tier == peer::Tier::Owner && !replica && !link.tier.is_hot(&key) {
                    parts.extensions.insert(Transient);
                }
                Some(Response::from_parts(
                    parts,
                    body.map_err(Into::into).boxed(),
                ))
            }
            _ => {
                stats.fallbacks.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(%addr, "tier fetch failed; going to the origin");
                None
            }
        }
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
        if let Some(resp) = self.send_to_tier(&parts, id, client).await {
            return (resp, Delivery::Upstream);
        }
        let declared = declared_length(&parts.headers);
        let has_body = !(parts.method == Method::GET || parts.method == Method::HEAD)
            || declared.is_some_and(|n| n > 0)
            || parts.headers.contains_key(header::TRANSFER_ENCODING);
        let limit = usize::try_from(self.settings.max_request_body).unwrap_or(usize::MAX);
        // A small body of an idempotent request is read whole first, so the
        // request can be sent again when a pooled connection to the origin
        // was closed under it (an origin's keep-alive timeout can end one
        // just as it is reused). Others stream, and are sent once.
        let replayable = declared.is_some_and(|n| n <= REPLAY_BODY_MAX)
            && !parts.headers.contains_key(header::TRANSFER_ENCODING)
            && parts.method.is_idempotent();
        let body = if !has_body {
            OutBody::None
        } else if replayable {
            match read_replayable(body).await {
                Ok(bytes) => OutBody::Replayable(bytes),
                Err(_) => {
                    return (
                        text(StatusCode::BAD_REQUEST, "request body incomplete\n"),
                        Delivery::Generated("client-body"),
                    );
                }
            }
        } else {
            OutBody::Stream(Limited::new(body, limit).boxed())
        };
        let out = Outbound {
            method: parts.method,
            path: path_of(&parts.uri),
            headers: self.outbound_headers(parts.headers, id, client),
            body,
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
        out.remove(HOP);
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
