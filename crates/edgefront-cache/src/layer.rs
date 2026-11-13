//! The cache as a layer on proxy routes: [`CacheLayer`] runs the cache's
//! part of a request in the [`edgefront_core::filter`] phases, with each
//! route's [`RoutePolicy`].
//!
//! - **Request phase.** The request is looked up. A fresh copy, or a
//!   stale one the client's `max-stale` accepts, is the answer. Otherwise
//!   the request goes upstream: conditional on the stored copy's
//!   validators when there is one, and without the client's own
//!   conditionals and `Range`, because the response is fetched for every
//!   client. `only-if-cached` with nothing usable is `504`. A request with
//!   `no-store`, a `HEAD` without a usable copy, and other methods pass
//!   through.
//! - **Upstream response phase.** A `304` refreshes the stored copy and
//!   the client gets that copy. A storable response is read up to the
//!   route's `max_object_bytes` within `body_timeout`, stored with the
//!   purge epoch taken in the request phase (a purge that lands in between
//!   wins) and served from the store, so the client's conditionals and
//!   ranges apply. A larger body streams through unstored. A successful
//!   unsafe method invalidates its URL and the same-host
//!   `Location`/`Content-Location`.
//! - **Deliver phase.** An upstream `500`, `502`, `503` or `504`, or an
//!   error the proxy or the layer wrote itself, is replaced by the stored
//!   copy when `stale-if-error` allows it. Every response gets
//!   `Cache-Status`.
//!
//! The layer does not collapse concurrent misses or refresh in the
//! background yet: a copy inside its `stale-while-revalidate` window is
//! revalidated before use, which RFC 5861 allows. `Via`, the request ID
//! and the forwarding headers belong to the proxy.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use bytes::{Bytes, BytesMut};
use edgefront_core::body::{Body, text};
use edgefront_core::filter::{
    BoxFuture, Decision, Delivery, Exchange, Filter, RequestHead, RouteFilters,
};
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, Response, StatusCode, Uri};
use http_body_util::BodyExt;
use hyper::body::{Body as HttpBody, Frame, SizeHint};

use crate::cache_control::{CacheControl, is_tchar};
use crate::config::{RoutePolicies, RoutePolicy};
use crate::headers::strip_hop_by_hop;
use crate::object::{CachedObject, Key, SURROGATE_KEY, primary_key};
use crate::policy::{self, Exchange as Exchanged, Freshness, Use};
use crate::purge::PurgeTarget;
use crate::respond;
use crate::store::Cache;

/// Wall-clock time, replaceable in tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

/// The system's wall clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Request headers that only make sense for the client that sent them, so
/// they are not forwarded on a fetch whose response every client may use.
pub const CLIENT_CONDITIONALS: [HeaderName; 6] = [
    header::IF_NONE_MATCH,
    header::IF_MODIFIED_SINCE,
    header::IF_MATCH,
    header::IF_UNMODIFIED_SINCE,
    header::IF_RANGE,
    header::RANGE,
];

/// The `Cache-Status` response field (RFC 9211).
pub const CACHE_STATUS: HeaderName = HeaderName::from_static("cache-status");

/// Origin statuses that count as errors for `stale-if-error` (RFC 5861
/// section 4).
pub fn is_error_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 500 | 502 | 503 | 504)
}

/// Settings of a [`CacheLayer`] that are not per route.
#[derive(Debug, Clone)]
pub struct LayerSettings {
    /// The cache's name in `Cache-Status`. A name that is not an HTTP
    /// token is replaced by `edgefront`.
    pub name: String,
    /// The longest an upstream body being stored may take to arrive.
    pub body_timeout: Duration,
}

impl Default for LayerSettings {
    fn default() -> Self {
        LayerSettings {
            name: "edgefront".into(),
            body_timeout: Duration::from_secs(30),
        }
    }
}

/// The cache layer: one store shared by every route it is attached to.
pub struct CacheLayer {
    shared: Arc<Shared>,
}

struct Shared {
    cache: Arc<Cache>,
    policies: RoutePolicies,
    clock: Arc<dyn Clock>,
    name: String,
    body_timeout: Duration,
}

impl CacheLayer {
    pub fn new(
        cache: Arc<Cache>,
        policies: RoutePolicies,
        settings: LayerSettings,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        let name = if !settings.name.is_empty() && settings.name.bytes().all(is_tchar) {
            settings.name
        } else {
            "edgefront".into()
        };
        Arc::new(CacheLayer {
            shared: Arc::new(Shared {
                cache,
                policies,
                clock,
                name,
                body_timeout: settings.body_timeout,
            }),
        })
    }

    pub fn cache(&self) -> &Arc<Cache> {
        &self.shared.cache
    }

    /// The name `Cache-Status` uses.
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// The layer attached to every route that has a cache policy, and no
    /// filter on the others, so they bypass the cache entirely.
    pub fn route_filters(self: &Arc<Self>) -> RouteFilters {
        let policies = &self.shared.policies;
        let mut filters = RouteFilters::new(policies.len());
        for i in 0..policies.len() {
            if policies.get(i).is_some() {
                filters.set(i, self.clone());
            }
        }
        filters
    }
}

impl Filter for CacheLayer {
    fn request<'a>(&'a self, req: RequestHead<'a>) -> BoxFuture<'a, Decision> {
        Box::pin(self.shared.request(req))
    }
}

impl Shared {
    fn status(&self, detail: &str) -> HeaderValue {
        HeaderValue::from_str(&format!("{}; {detail}", self.name))
            .unwrap_or_else(|_| HeaderValue::from_static("edgefront"))
    }

    fn hit(&self, obj: &CachedObject, method: &Method, req: &HeaderMap, detail: &str) -> Decision {
        let now = self.clock.now();
        let mut resp = respond::from_cache(obj, method, req, now);
        let status = format!("hit; ttl={}{detail}", obj.freshness.ttl_secs(now));
        resp.headers_mut()
            .insert(CACHE_STATUS, self.status(&status));
        Decision::Respond(resp)
    }

    async fn request(self: &Arc<Self>, req: RequestHead<'_>) -> Decision {
        let Some(policy) = self.policies.get(req.route) else {
            return Decision::Forward(None);
        };
        let Some(key) = primary_key(req.uri, req.headers) else {
            return Decision::Forward(None);
        };
        let req_cc = CacheControl::from_headers(req.headers);
        let method = req.method;
        if method != Method::GET && method != Method::HEAD {
            return self.pass(key, &req, "fwd=method");
        }
        if req_cc.no_store {
            return self.pass(key, &req, "fwd=request");
        }
        let now = self.clock.now();
        let stored = self.cache.lookup(&key, req.headers).await;
        let usable = stored
            .as_ref()
            .map(|obj| (obj, policy::decide(&obj.freshness, &obj.cc, &req_cc, now)));
        match usable {
            Some((obj, Use::Fresh)) => return self.hit(obj, method, req.headers, ""),
            Some((obj, Use::StaleAccepted)) => {
                return self.hit(obj, method, req.headers, "; detail=max-stale");
            }
            _ if *method == Method::HEAD => return self.pass(key, &req, "fwd=miss"),
            _ if req_cc.only_if_cached => {
                let mut resp = text(StatusCode::GATEWAY_TIMEOUT, "not in cache\n");
                resp.headers_mut()
                    .insert(CACHE_STATUS, self.status("fwd=miss; detail=only-if-cached"));
                return Decision::Respond(resp);
            }
            _ => {}
        }
        let fwd = match &stored {
            None => "fwd=miss",
            Some(obj) if obj.freshness.is_fresh(now) => "fwd=request",
            Some(_) => "fwd=stale",
        };
        let client_headers = req.headers.clone();
        for name in &CLIENT_CONDITIONALS {
            req.headers.remove(name);
        }
        if let Some(obj) = &stored {
            for (name, value) in policy::conditional_headers(&obj.headers) {
                req.headers.insert(name, value);
            }
        }
        Decision::Forward(Some(Box::new(Fill {
            shared: self.clone(),
            policy: policy.clone(),
            key,
            client_headers,
            req_cc,
            stored,
            epoch: self.cache.epoch(),
            request_time: now,
            fwd,
            status: String::new(),
            failure: None,
        })))
    }

    fn pass(self: &Arc<Self>, key: Key, req: &RequestHead<'_>, fwd: &'static str) -> Decision {
        let unsafe_method = !matches!(
            *req.method,
            Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
        );
        Decision::Forward(Some(Box::new(Pass {
            shared: self.clone(),
            invalidate: unsafe_method.then(|| Invalidate {
                key,
                uri: req.uri.clone(),
                headers: req.headers.clone(),
            }),
            fwd,
            status: String::new(),
        })))
    }

    fn store(&self, key: &Key, obj: &Arc<CachedObject>, fill: &Fill) -> bool {
        self.cache.store_limited(
            key.clone(),
            obj.clone(),
            &fill.client_headers,
            fill.epoch,
            fill.policy.max_variants,
        )
    }
}

/// What an unsafe method that succeeds invalidates (RFC 9111 section 4.4).
struct Invalidate {
    key: Key,
    uri: Uri,
    headers: HeaderMap,
}

/// A request the cache passes through.
struct Pass {
    shared: Arc<Shared>,
    invalidate: Option<Invalidate>,
    fwd: &'static str,
    status: String,
}

impl Exchange for Pass {
    fn upstream_response(&mut self, resp: Response<Body>) -> BoxFuture<'_, Response<Body>> {
        Box::pin(async move {
            let (mut parts, body) = resp.into_parts();
            parts.headers.remove(SURROGATE_KEY);
            let ok = parts.status.is_success() || parts.status.is_redirection();
            if let Some(inv) = self.invalidate.take().filter(|_| ok) {
                let mut targets = vec![PurgeTarget::Key(inv.key.clone())];
                for name in [header::LOCATION, header::CONTENT_LOCATION] {
                    if let Some(other) =
                        same_host_key(&inv.key, &inv.uri, &inv.headers, parts.headers.get(&name))
                    {
                        targets.push(PurgeTarget::Key(other));
                    }
                }
                self.shared.cache.purge(&targets).await;
            }
            self.status = format!("{}; fwd-status={}", self.fwd, parts.status.as_u16());
            Response::from_parts(parts, body)
        })
    }

    fn deliver(self: Box<Self>, mut resp: Response<Body>, from: Delivery) -> Response<Body> {
        let status = match from {
            Delivery::Upstream => self.status,
            Delivery::Generated(reason) => format!("{}; detail={reason}", self.fwd),
        };
        resp.headers_mut()
            .insert(CACHE_STATUS, self.shared.status(&status));
        resp
    }
}

/// A GET the cache may fill from the upstream response.
struct Fill {
    shared: Arc<Shared>,
    policy: RoutePolicy,
    key: Key,
    /// The client's request headers, for Vary, storability and serving.
    client_headers: HeaderMap,
    req_cc: CacheControl,
    /// The stored copy being revalidated, if any.
    stored: Option<Arc<CachedObject>>,
    /// The purge epoch before the request went upstream.
    epoch: u64,
    request_time: SystemTime,
    fwd: &'static str,
    /// `Cache-Status` for an upstream response.
    status: String,
    /// Why the upstream response counts as an error, if it does.
    failure: Option<String>,
}

impl Fill {
    fn freshness(&self, status: StatusCode, headers: &HeaderMap, cc: &CacheControl) -> Freshness {
        let mut f = Freshness::compute(
            status,
            headers,
            cc,
            self.request_time,
            self.shared.clock.now(),
            self.policy.heuristic_max,
        );
        if let Some(cap) = self.policy.max_ttl {
            f.lifetime = f.lifetime.min(cap);
        }
        f
    }

    fn serve(&self, obj: &CachedObject) -> Response<Body> {
        respond::from_cache(
            obj,
            &Method::GET,
            &self.client_headers,
            self.shared.clock.now(),
        )
    }

    /// The stored copy in place of an upstream failure, when
    /// `stale-if-error` allows it.
    fn stale_on_error(&self, failure: &str) -> Option<Response<Body>> {
        let obj = self.stored.as_ref()?;
        let now = self.shared.clock.now();
        if !policy::stale_if_error_allows(&obj.freshness, &obj.cc, &self.req_cc, now) {
            return None;
        }
        let mut resp = self.serve(obj);
        let status = format!(
            "hit; ttl={}; fwd=stale; {failure}; detail=stale-if-error",
            obj.freshness.ttl_secs(now)
        );
        resp.headers_mut()
            .insert(CACHE_STATUS, self.shared.status(&status));
        Some(resp)
    }

    /// A response that goes to the client as the upstream sent it.
    fn through(&mut self, mut parts: http::response::Parts, body: Body) -> Response<Body> {
        parts.headers.remove(SURROGATE_KEY);
        self.status = format!("{}; fwd-status={}", self.fwd, parts.status.as_u16());
        Response::from_parts(parts, body)
    }

    /// An upstream response the layer could not use, answered with an
    /// error of its own.
    fn failed(
        &mut self,
        status: StatusCode,
        message: &'static str,
        detail: &str,
    ) -> Response<Body> {
        self.failure = Some(format!("detail-origin={detail}"));
        self.status = format!("{}; detail={detail}", self.fwd);
        text(status, message)
    }

    async fn not_modified(
        &mut self,
        stored: Arc<CachedObject>,
        parts: &http::response::Parts,
    ) -> Response<Body> {
        if !policy::not_modified_matches(&stored.headers, &parts.headers) {
            // The 304 is about another representation than the one asked
            // about. It cannot refresh the copy, and the client sent no
            // conditional it could answer; drop the copy so the next
            // request fetches in full.
            self.shared
                .cache
                .purge(&[PurgeTarget::Key(self.key.clone())])
                .await;
            return self.failed(
                StatusCode::BAD_GATEWAY,
                "origin response invalid\n",
                "mismatched-304",
            );
        }
        let headers = policy::merge_not_modified(&stored.headers, &parts.headers);
        let cc = CacheControl::from_headers(&headers);
        let freshness = self.freshness(stored.status, &headers, &cc);
        let obj = Arc::new(CachedObject::with_vary(
            stored.status,
            headers,
            stored.body.clone(),
            freshness,
            stored.vary().clone(),
        ));
        let kept = self.shared.store(&self.key, &obj, self);
        self.status = format!(
            "{}; fwd-status=304{}; ttl={}",
            self.fwd,
            if kept { "; stored" } else { "" },
            obj.freshness.ttl_secs(self.shared.clock.now())
        );
        self.serve(&obj)
    }

    async fn fill(&mut self, resp: Response<Body>) -> Response<Body> {
        let (mut parts, body) = resp.into_parts();
        strip_hop_by_hop(&mut parts.headers);
        if parts.status == StatusCode::NOT_MODIFIED
            && let Some(stored) = self.stored.clone()
        {
            return self.not_modified(stored, &parts).await;
        }
        if is_error_status(parts.status) {
            self.failure = Some(format!("fwd-status={}", parts.status.as_u16()));
        }
        let resp_cc = CacheControl::from_headers(&parts.headers);
        let exchange = Exchanged {
            method: &Method::GET,
            req_headers: &self.client_headers,
            req_cc: &self.req_cc,
            status: parts.status,
            resp_headers: &parts.headers,
            resp_cc: &resp_cc,
            response_time: self.shared.clock.now(),
        };
        if policy::storable(&exchange, self.policy.heuristic_max).is_err() {
            // The origin answered in full but no longer lets this be
            // stored. Drop the old copy too, or stale-if-error could keep
            // serving what the origin has since made private.
            if self.stored.is_some() && !is_error_status(parts.status) {
                self.shared
                    .cache
                    .purge(&[PurgeTarget::Key(self.key.clone())])
                    .await;
            }
            return self.through(parts, body);
        }
        let limit = usize::try_from(self.policy.max_object_bytes)
            .unwrap_or(usize::MAX)
            .min(self.shared.cache.memory().max_object_bytes());
        let declared = parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
        if declared.is_some_and(|n| n > limit as u64) {
            return self.through(parts, body);
        }
        let read = tokio::time::timeout(self.shared.body_timeout, read_limited(body, limit)).await;
        let bytes = match read {
            Ok(Ok(Read::Complete(bytes))) => bytes,
            Ok(Ok(Read::TooLarge(prefix, rest))) => {
                let body = Prefixed {
                    prefix: Some(prefix),
                    rest,
                }
                .boxed();
                return self.through(parts, body);
            }
            Ok(Err(_)) => {
                return self.failed(
                    StatusCode::BAD_GATEWAY,
                    "origin response invalid\n",
                    "protocol",
                );
            }
            Err(_) => {
                return self.failed(StatusCode::GATEWAY_TIMEOUT, "origin timed out\n", "timeout");
            }
        };
        // The body is complete now; framing headers describe the stored copy.
        parts.headers.remove(header::CONTENT_LENGTH);
        let freshness = self.freshness(parts.status, &parts.headers, &resp_cc);
        let obj = Arc::new(CachedObject::new(
            parts.status,
            parts.headers,
            bytes,
            freshness,
            &self.client_headers,
        ));
        let kept = self.shared.store(&self.key, &obj, self);
        self.status = format!(
            "{}; fwd-status={}{}; ttl={}",
            self.fwd,
            obj.status.as_u16(),
            if kept { "; stored" } else { "" },
            obj.freshness.ttl_secs(self.shared.clock.now())
        );
        self.serve(&obj)
    }
}

impl Exchange for Fill {
    fn upstream_response(&mut self, resp: Response<Body>) -> BoxFuture<'_, Response<Body>> {
        Box::pin(self.fill(resp))
    }

    fn deliver(self: Box<Self>, mut resp: Response<Body>, from: Delivery) -> Response<Body> {
        let failure = match from {
            Delivery::Upstream => self.failure.clone(),
            Delivery::Generated(reason) => Some(format!("detail-origin={reason}")),
        };
        if let Some(failure) = &failure
            && let Some(stale) = self.stale_on_error(failure)
        {
            return stale;
        }
        let status = match from {
            Delivery::Upstream => self.status,
            Delivery::Generated(reason) => format!("{}; detail={reason}", self.fwd),
        };
        resp.headers_mut()
            .insert(CACHE_STATUS, self.shared.status(&status));
        resp
    }
}

/// The cache key for a `Location` or `Content-Location` value on the same
/// host as the request.
pub fn same_host_key(
    key: &Key,
    uri: &Uri,
    headers: &HeaderMap,
    value: Option<&HeaderValue>,
) -> Option<Key> {
    let target: Uri = value?.to_str().ok()?.parse().ok()?;
    let own = primary_key(uri, headers)?;
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    let host = &own[..own.len() - path.len()];
    let target_path = target.path_and_query().map_or("/", |p| p.as_str());
    let other = match target.authority() {
        Some(a) if a.as_str().eq_ignore_ascii_case(host) => format!("{host}{target_path}"),
        Some(_) => return None,
        None if target.path().starts_with('/') => format!("{host}{target_path}"),
        None => return None,
    };
    (other.as_str() != &**key).then(|| other.into())
}

/// A body read up to a limit.
pub enum Read<B> {
    Complete(Bytes),
    /// Over the limit: what was read, and the rest of the stream.
    TooLarge(Bytes, B),
}

/// Reads a body up to `limit` bytes. Past the limit, hands back what was
/// read and the rest of the stream instead of buffering more.
pub async fn read_limited<B>(mut body: B, limit: usize) -> Result<Read<B>, B::Error>
where
    B: HttpBody<Data = Bytes> + Unpin,
{
    let mut buf = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let Ok(data) = frame?.into_data() else {
            continue;
        };
        buf.extend_from_slice(&data);
        if buf.len() > limit {
            return Ok(Read::TooLarge(buf.freeze(), body));
        }
    }
    Ok(Read::Complete(buf.freeze()))
}

/// A body that yields a buffered prefix, then the rest of a stream.
pub struct Prefixed<B> {
    pub prefix: Option<Bytes>,
    pub rest: B,
}

impl<B> HttpBody for Prefixed<B>
where
    B: HttpBody<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        if let Some(prefix) = self.prefix.take() {
            return Poll::Ready(Some(Ok(Frame::data(prefix))));
        }
        Pin::new(&mut self.rest).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.prefix.is_none() && self.rest.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        let rest = self.rest.size_hint();
        let Some(p) = &self.prefix else {
            return rest;
        };
        let n = p.len() as u64;
        // Upper first: setting a lower bound above the current upper one
        // panics.
        let mut hint = SizeHint::new();
        if let Some(upper) = rest.upper() {
            hint.set_upper(upper.saturating_add(n));
        }
        hint.set_lower(rest.lower().saturating_add(n));
        hint
    }
}
