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
//!   the client gets that copy; a `304` about another representation than
//!   the one asked about is answered by fetching again without validators
//!   ([`Upstream::Resend`]). A storable response is read up to the route's
//!   `max_object_bytes` within `body_timeout`, stored with the purge epoch
//!   taken in the request phase (a purge that lands in between wins) and
//!   served from the store, so the client's conditionals and ranges apply.
//!   A larger body streams through unstored. A successful unsafe method
//!   invalidates its URL and the same-host `Location`/`Content-Location`.
//! - **Deliver phase.** An upstream `500`, `502`, `503` or `504`, or an
//!   error the proxy or the layer wrote itself, is replaced by the stored
//!   copy when `stale-if-error` allows it. Every response gets
//!   `Cache-Status`, and an [`Outcome`] in its extensions for the server's
//!   logs and metrics.
//!
//! Concurrent fills of one URL collapse into one upstream request. The
//! first request leads: it goes upstream and publishes what came back.
//! The others wait for that in their request phase and answer from it
//! (`collapsed` in `Cache-Status`), share its failure, or go upstream on
//! their own when the result was only for the leader: a response that
//! could not be stored, or a variant their own request would not select.
//! The leader's fetch is part of its request, so a leader whose client
//! goes away releases its followers and one of them leads instead.
//!
//! A copy inside its `stale-while-revalidate` window is served at once
//! while the layer refreshes it upstream in the background
//! ([`Decision::Refresh`]), one refresh per URL at a time.
//!
//! The request ID and the forwarding headers belong to the server. `Via`
//! does too, except on responses that pass through the cache unstored,
//! which get [`LayerSettings::via`] when it is set.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use bytes::{Bytes, BytesMut};
use edgefront_core::body::{Body, text};
use edgefront_core::filter::{
    BoxFuture, Decision, Delivery, Exchange, Filter, RequestHead, RouteFilters, Upstream,
};
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, Response, StatusCode, Uri};
use http_body_util::BodyExt;
use hyper::body::{Body as HttpBody, Frame, SizeHint};

use crate::cache_control::{CacheControl, is_tchar};
use crate::collapse::{Collapser, Flight, Lead};
use crate::config::{RoutePolicies, RoutePolicy};
use crate::fill::{End, Filling};
use crate::headers::strip_hop_by_hop;
use crate::object::{CachedObject, Key, SURROGATE_KEY, primary_key};
use crate::policy::{self, Exchange as Exchanged, Freshness, NotStorable, Use};
use crate::purge::PurgeTarget;
use crate::respond;
use crate::slice::{self, Layout, Progress, SLICES, Slicer};
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

/// A fill streams to its clients while it arrives when the origin declares
/// a body longer than this. Smaller bodies are read whole first, which costs
/// a client almost nothing and keeps one code path for them.
pub const STREAM_ABOVE: u64 = 64 << 10;

/// Whether a client request carries conditionals or a range: such a
/// request is answered from the complete object, never a stream.
fn needs_whole(headers: &HeaderMap) -> bool {
    CLIENT_CONDITIONALS
        .iter()
        .any(|name| headers.contains_key(name))
}

/// The client response for a stream of `head`'s body: the headers a hit
/// would carry, the declared length, and the growing body.
fn streamed(head: &CachedObject, fill: &Arc<Filling>, len: u64, now: SystemTime) -> Response<Body> {
    let (mut parts, _) =
        respond::from_cache(head, &Method::GET, &HeaderMap::new(), now).into_parts();
    parts
        .headers
        .insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    Response::from_parts(parts, fill.reader().boxed())
}

/// The `Cache-Status` response field (RFC 9211).
pub const CACHE_STATUS: HeaderName = HeaderName::from_static("cache-status");

/// Origin statuses that count as errors for `stale-if-error` (RFC 5861
/// section 4).
pub fn is_error_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 500 | 502 | 503 | 504)
}

/// How a request was answered. The layer puts one in the extensions of
/// every response it makes or passes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Fresh from the cache.
    Hit,
    /// Stale from the cache while a refresh runs, or accepted by max-stale.
    Stale,
    /// Stale from the cache because the upstream failed.
    StaleIfError,
    /// Fetched from upstream and possibly stored.
    Miss,
    /// Revalidated with a 304.
    Revalidated,
    /// Not cacheable: passed through to upstream.
    Pass,
    /// Answered with an error the cache or the server wrote.
    Error,
}

impl Outcome {
    pub const ALL: [Outcome; 7] = [
        Outcome::Hit,
        Outcome::Stale,
        Outcome::StaleIfError,
        Outcome::Miss,
        Outcome::Revalidated,
        Outcome::Pass,
        Outcome::Error,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Hit => "hit",
            Outcome::Stale => "stale",
            Outcome::StaleIfError => "stale_if_error",
            Outcome::Miss => "miss",
            Outcome::Revalidated => "revalidated",
            Outcome::Pass => "pass",
            Outcome::Error => "error",
        }
    }
}

/// Events the layer reports beyond each response's [`Outcome`], for a
/// server's metrics. Every method does nothing by default.
pub trait Observer: Send + Sync {
    /// An upstream response that may not be stored, and why.
    fn not_stored(&self, _reason: NotStorable) {}
    /// A storable response too large to store, sent on as it streamed.
    fn streamed_uncached(&self) {}
    /// A request that waited for another one's fill.
    fn collapsed(&self) {}
    /// A stale copy refreshed in the background.
    fn background_refresh(&self) {}
}

struct Quiet;

impl Observer for Quiet {}

/// Settings of a [`CacheLayer`] that are not per route.
#[derive(Debug, Clone)]
pub struct LayerSettings {
    /// The cache's name in `Cache-Status`. A name that is not an HTTP
    /// token is replaced by `edgefront`.
    pub name: String,
    /// The longest an upstream body being stored may take to arrive.
    pub body_timeout: Duration,
    /// A `Via` value appended to the responses that pass through the cache
    /// without being stored, as a caching intermediary must (RFC 9110
    /// section 7.6.3). `None` leaves `Via` to the server.
    pub via: Option<HeaderValue>,
}

impl Default for LayerSettings {
    fn default() -> Self {
        LayerSettings {
            name: "edgefront".into(),
            body_timeout: Duration::from_secs(30),
            via: None,
        }
    }
}

/// The message of an error a follower shares, for the reason the server
/// gave the leader's.
fn failure_message(reason: &str) -> &'static str {
    match reason {
        "connect" => "origin unreachable\n",
        "timeout" => "origin timed out\n",
        "protocol" => "origin response invalid\n",
        "body-too-large" => "request body too large\n",
        _ => "upstream failed\n",
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
    via: Option<HeaderValue>,
    collapser: Arc<Collapser<Fetched>>,
    observer: Arc<dyn Observer>,
}

/// What a leading fill got from upstream, as its followers see it.
#[derive(Clone)]
enum Fetched {
    /// A stored response every follower whose request selects it may use.
    Object {
        obj: Arc<CachedObject>,
        stored: bool,
        /// Refreshed by a `304` rather than a full response.
        revalidated: bool,
    },
    /// A response every follower may read while it arrives: the head of
    /// the object being filled, and its growing body.
    Streaming {
        head: Arc<CachedObject>,
        fill: Arc<Filling>,
    },
    /// A large object being stored as slices: its manifest and the fill's
    /// progress, which followers read from.
    Sliced {
        head: Arc<CachedObject>,
        progress: Arc<Progress>,
    },
    /// A response only the leader could use: not storable, or too large.
    Single(StatusCode),
    /// No usable upstream response.
    Failed {
        status: StatusCode,
        message: &'static str,
        detail: String,
    },
}

impl CacheLayer {
    pub fn new(
        cache: Arc<Cache>,
        policies: RoutePolicies,
        settings: LayerSettings,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        CacheLayer::with_observer(cache, policies, settings, clock, Arc::new(Quiet))
    }

    /// [`CacheLayer::new`] reporting to `observer`.
    pub fn with_observer(
        cache: Arc<Cache>,
        policies: RoutePolicies,
        settings: LayerSettings,
        clock: Arc<dyn Clock>,
        observer: Arc<dyn Observer>,
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
                via: settings.via,
                collapser: Arc::default(),
                observer,
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

    /// Fills and background refreshes running upstream now.
    pub fn in_flight(&self) -> usize {
        self.shared.collapser.in_flight()
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

/// The byte range a request asks of a sliced object of `total` bytes: the
/// status, the first and the last byte. `Err` is the 416 to send.
fn slice_range(
    headers: &HeaderMap,
    etag: Option<&crate::headers::EntityTag>,
    total: u64,
) -> Result<(StatusCode, u64, u64), Box<Response<Body>>> {
    let whole = Ok((StatusCode::OK, 0, total.saturating_sub(1)));
    let Some(range) = headers.get(header::RANGE) else {
        return whole;
    };
    // If-Range: only the stored entity tag, compared strongly.
    if let Some(cond) = headers.get(header::IF_RANGE) {
        let holds = crate::headers::EntityTag::parse(cond.as_bytes())
            .is_some_and(|t| etag.is_some_and(|e| e.strong_eq(&t)));
        if !holds {
            return whole;
        }
    }
    match crate::range::parse(range.as_bytes(), total) {
        crate::range::ByteRange::Satisfiable { start, end } => {
            Ok((StatusCode::PARTIAL_CONTENT, start, end))
        }
        crate::range::ByteRange::Unsatisfiable => {
            let mut resp = Response::new(edgefront_core::body::empty());
            *resp.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{total}")) {
                resp.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            resp.headers_mut()
                .insert(header::CONTENT_LENGTH, HeaderValue::from(0));
            Err(Box::new(resp))
        }
        // Several ranges of a sliced object are answered with the whole.
        _ => whole,
    }
}

/// A response for bytes `start..=end` of the sliced object `head` with
/// `body`, or no body for HEAD.
fn sliced_response(
    head: &CachedObject,
    status: StatusCode,
    start: u64,
    end: u64,
    total: u64,
    body: Option<Body>,
    now: SystemTime,
) -> Response<Body> {
    let mut headers = head.headers.clone();
    headers.remove(&SLICES);
    headers.remove(SURROGATE_KEY);
    headers.insert(
        header::AGE,
        HeaderValue::from(head.freshness.current_age(now).as_secs()),
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let len = if total == 0 { 0 } else { end - start + 1 };
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if status == StatusCode::PARTIAL_CONTENT
        && let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{total}"))
    {
        headers.insert(header::CONTENT_RANGE, v);
    }
    let mut resp = Response::new(body.unwrap_or_else(edgefront_core::body::empty));
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    resp
}

impl Shared {
    /// Answers a request for the sliced object `obj` from its slices, or
    /// says what to fetch: `Err(Some(range))` the slices a range needs,
    /// `Err(None)` the whole object, when some are no longer held.
    fn sliced_hit(
        &self,
        obj: &Arc<CachedObject>,
        layout: Layout,
        method: &Method,
        req: &HeaderMap,
        key: &Key,
        now: SystemTime,
    ) -> Result<Response<Body>, Option<HeaderValue>> {
        // Preconditions first, from the manifest's headers.
        let pre = respond::from_cache(obj, &Method::HEAD, req, now);
        if matches!(
            pre.status(),
            StatusCode::NOT_MODIFIED | StatusCode::PRECONDITION_FAILED
        ) {
            let mut resp = pre;
            resp.headers_mut().remove(&SLICES);
            self.mark(
                &mut resp,
                &format!("hit; ttl={}", obj.freshness.ttl_secs(now)),
                Outcome::Hit,
            );
            return Ok(resp);
        }
        let ranged = if *method == Method::GET {
            slice_range(req, obj.etag.as_ref(), layout.total)
        } else {
            Ok((StatusCode::OK, 0, layout.total.saturating_sub(1)))
        };
        let (status, start, end) = match ranged {
            Ok(r) => r,
            Err(mut resp) => {
                self.mark(
                    &mut resp,
                    &format!("hit; ttl={}", obj.freshness.ttl_secs(now)),
                    Outcome::Hit,
                );
                return Ok(*resp);
            }
        };
        if layout.total > 0 {
            let needed = layout.covering(start, end);
            let missing = needed
                .clone()
                .find(|i| !self.cache.contains(&slice::slice_key(key, *i)));
            if let Some(first) = missing {
                if status == StatusCode::OK {
                    return Err(None);
                }
                let last = *needed.end();
                let to = (layout.start_of(last) + layout.len_of(last)) - 1;
                let range =
                    HeaderValue::from_str(&format!("bytes={}-{to}", layout.start_of(first)))
                        .expect("ASCII digits");
                return Err(Some(range));
            }
        }
        let body = (*method == Method::GET && layout.total > 0).then(|| {
            slice::read(
                self.cache.clone(),
                key.clone(),
                layout,
                start,
                end,
                obj.etag.clone(),
                None,
            )
            .boxed()
        });
        let mut resp = sliced_response(obj, status, start, end, layout.total, body, now);
        self.mark(
            &mut resp,
            &format!("hit; ttl={}", obj.freshness.ttl_secs(now)),
            Outcome::Hit,
        );
        Ok(resp)
    }

    /// Sets the response's `Cache-Status` and [`Outcome`].
    fn mark(&self, resp: &mut Response<Body>, detail: &str, outcome: Outcome) {
        let value = HeaderValue::from_str(&format!("{}; {detail}", self.name))
            .unwrap_or_else(|_| HeaderValue::from_static("edgefront"));
        resp.headers_mut().insert(CACHE_STATUS, value);
        resp.extensions_mut().insert(outcome);
    }

    fn hit(
        &self,
        obj: &CachedObject,
        method: &Method,
        req: &HeaderMap,
        detail: &str,
        outcome: Outcome,
    ) -> Response<Body> {
        let now = self.clock.now();
        let mut resp = respond::from_cache(obj, method, req, now);
        let status = format!("hit; ttl={}{detail}", obj.freshness.ttl_secs(now));
        self.mark(&mut resp, &status, outcome);
        resp
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
        let mut stored = self.cache.lookup(&key, req.headers).await;
        // A large object stored as slices: served from them, or the slices
        // it lacks fetched with Range. A stale manifest is not revalidated
        // but fetched again.
        let mut slice_fetch: Option<(Arc<CachedObject>, Option<HeaderValue>)> = None;
        if let Some(obj) = stored.clone()
            && let Some(layout) = Layout::of(&obj)
        {
            let usable = policy::decide(&obj.freshness, &obj.cc, &req_cc, now);
            if matches!(usable, Use::Fresh | Use::StaleAccepted) {
                match self.sliced_hit(&obj, layout, method, req.headers, &key, now) {
                    Ok(resp) => return Decision::Respond(resp),
                    Err(range) => slice_fetch = Some((obj, range)),
                }
            }
            stored = None;
        }
        let usable = stored
            .as_ref()
            .map(|obj| (obj, policy::decide(&obj.freshness, &obj.cc, &req_cc, now)));
        match usable {
            Some((obj, Use::Fresh)) => {
                return Decision::Respond(self.hit(obj, method, req.headers, "", Outcome::Hit));
            }
            // A HEAD answered from a copy max-stale accepts is reported as
            // a plain hit, as cachet 0.1 did.
            Some((obj, Use::StaleAccepted)) if *method == Method::HEAD => {
                return Decision::Respond(self.hit(obj, method, req.headers, "", Outcome::Hit));
            }
            Some((obj, Use::StaleAccepted)) => {
                let resp = self.hit(
                    obj,
                    method,
                    req.headers,
                    "; detail=max-stale",
                    Outcome::Stale,
                );
                return Decision::Respond(resp);
            }
            _ if *method == Method::HEAD => return self.pass(key, &req, "fwd=miss"),
            _ if req_cc.only_if_cached => {
                let mut resp = text(StatusCode::GATEWAY_TIMEOUT, "not in cache\n");
                self.mark(&mut resp, "fwd=miss; detail=only-if-cached", Outcome::Error);
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
        if let Some((_, Some(range))) = &slice_fetch {
            req.headers.insert(header::RANGE, range.clone());
        }
        let forwarded = req.headers.clone();
        let fill = |lead| {
            Box::new(Fill {
                shared: self.clone(),
                policy: policy.clone(),
                key: key.clone(),
                client_headers: client_headers.clone(),
                forwarded: forwarded.clone(),
                req_cc: req_cc.clone(),
                stored: stored.clone(),
                resent: false,
                epoch: self.cache.epoch(),
                request_time: now,
                fwd,
                status: String::new(),
                outcome: Outcome::Miss,
                failure: None,
                lead,
                manifest: slice_fetch.as_ref().map(|(obj, _)| obj.clone()),
            })
        };
        if let Some((obj, Use::StaleWhileRevalidate)) = usable {
            let resp = self.hit(
                obj,
                method,
                &client_headers,
                "; detail=stale-while-revalidate",
                Outcome::Stale,
            );
            return match self.collapser.lead_or_follow(key.clone()) {
                Flight::Lead(lead) => {
                    self.observer.background_refresh();
                    Decision::Refresh(resp, fill(Some(lead)))
                }
                // A refresh or fill is already on its way.
                Flight::Follow(_) => Decision::Respond(resp),
            };
        }
        loop {
            let follow = match self.collapser.lead_or_follow(key.clone()) {
                Flight::Lead(lead) => return Decision::Forward(Some(fill(Some(lead)))),
                Flight::Follow(follow) => follow,
            };
            // A leader that went away without a result: try again, and
            // perhaps lead this time.
            let Ok(fetched) = follow.wait().await else {
                continue;
            };
            self.observer.collapsed();
            let follower = Follower {
                shared: self,
                key: &key,
                client_headers: &client_headers,
                req_cc: &req_cc,
                stored: stored.as_ref(),
                fwd,
            };
            return match follower.answer(fetched).await {
                Some(resp) => Decision::Respond(resp),
                // Go upstream alone; nobody waits on this fetch.
                None => Decision::Forward(Some(fill(None))),
            };
        }
    }

    /// The stored copy in place of an upstream failure, when
    /// `stale-if-error` allows it.
    fn stale_on_error(
        &self,
        stored: Option<&Arc<CachedObject>>,
        req_cc: &CacheControl,
        client_headers: &HeaderMap,
        failure: &str,
    ) -> Option<Response<Body>> {
        let obj = stored?;
        let now = self.clock.now();
        if !policy::stale_if_error_allows(&obj.freshness, &obj.cc, req_cc, now) {
            return None;
        }
        let mut resp = respond::from_cache(obj, &Method::GET, client_headers, now);
        let status = format!(
            "hit; ttl={}; fwd=stale; {failure}; detail=stale-if-error",
            obj.freshness.ttl_secs(now)
        );
        self.mark(&mut resp, &status, Outcome::StaleIfError);
        Some(resp)
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

    /// Removes framing and stored-only fields from a response that passes
    /// through unstored, and adds `Via`.
    fn pass_on(&self, headers: &mut HeaderMap) {
        headers.remove(SURROGATE_KEY);
        if let Some(via) = &self.via {
            headers.append(header::VIA, via.clone());
        }
    }
}

/// A request that waited for another one's fill.
struct Follower<'a> {
    shared: &'a Shared,
    key: &'a Key,
    client_headers: &'a HeaderMap,
    req_cc: &'a CacheControl,
    stored: Option<&'a Arc<CachedObject>>,
    fwd: &'static str,
}

impl Follower<'_> {
    /// This request's answer from the leader's result, or `None` when it
    /// has to go upstream itself.
    async fn answer(&self, fetched: Fetched) -> Option<Response<Body>> {
        let fetched = match fetched {
            Fetched::Streaming { head, fill } => {
                if !needs_whole(self.client_headers) && head.matches(self.client_headers) {
                    let now = self.shared.clock.now();
                    let len = fill.expected().unwrap_or(0);
                    let mut resp = streamed(&head, &fill, len, now);
                    let status = format!(
                        "{}; fwd-status={}; stored; collapsed; ttl={}",
                        self.fwd,
                        head.status.as_u16(),
                        head.freshness.ttl_secs(now)
                    );
                    self.shared.mark(&mut resp, &status, Outcome::Miss);
                    return Some(resp);
                }
                // Conditionals, a range or another variant: answered from
                // the whole object once it is in.
                match fill.ended().await {
                    End::Complete(Some(obj)) => Fetched::Object {
                        obj,
                        stored: true,
                        revalidated: false,
                    },
                    End::Complete(None) => return None,
                    End::Aborted(detail) => Fetched::Failed {
                        status: StatusCode::BAD_GATEWAY,
                        message: "origin response invalid\n",
                        detail: detail.to_string(),
                    },
                }
            }
            Fetched::Sliced { head, progress } => {
                let others = CLIENT_CONDITIONALS
                    .iter()
                    .filter(|n| **n != header::RANGE && **n != header::IF_RANGE)
                    .any(|n| self.client_headers.contains_key(n));
                if others || !head.matches(self.client_headers) {
                    return None;
                }
                let now = self.shared.clock.now();
                let layout = progress.layout;
                let mut resp =
                    match slice_range(self.client_headers, head.etag.as_ref(), layout.total) {
                        Err(resp) => *resp,
                        Ok((status, start, end)) => {
                            if !progress.covers(start, end) {
                                return None;
                            }
                            let body = (layout.total > 0).then(|| {
                                slice::read(
                                    self.shared.cache.clone(),
                                    self.key.clone(),
                                    layout,
                                    start,
                                    end,
                                    head.etag.clone(),
                                    Some(progress.clone()),
                                )
                                .boxed()
                            });
                            sliced_response(&head, status, start, end, layout.total, body, now)
                        }
                    };
                let status = format!(
                    "{}; fwd-status=200; stored; collapsed; ttl={}",
                    self.fwd,
                    head.freshness.ttl_secs(now)
                );
                self.shared.mark(&mut resp, &status, Outcome::Miss);
                return Some(resp);
            }
            other => other,
        };
        let failure = match &fetched {
            Fetched::Failed { detail, .. } => Some(format!("detail-origin={detail}")),
            Fetched::Object { obj, .. } if is_error_status(obj.status) => {
                Some(format!("fwd-status={}", obj.status.as_u16()))
            }
            Fetched::Single(status) if is_error_status(*status) => {
                Some(format!("fwd-status={}", status.as_u16()))
            }
            _ => None,
        };
        if let Some(failure) = failure
            && let Some(stale) =
                self.shared
                    .stale_on_error(self.stored, self.req_cc, self.client_headers, &failure)
        {
            return Some(stale);
        }
        let fwd = self.fwd;
        let (mut resp, status, outcome) = match fetched {
            Fetched::Object {
                obj,
                stored,
                revalidated,
            } if obj.matches(self.client_headers) => {
                let now = self.shared.clock.now();
                let resp = respond::from_cache(&obj, &Method::GET, self.client_headers, now);
                let (fwd_status, outcome) = if revalidated {
                    (304, Outcome::Revalidated)
                } else {
                    (obj.status.as_u16(), Outcome::Miss)
                };
                let stored = if stored { "; stored" } else { "" };
                let ttl = obj.freshness.ttl_secs(now);
                let status =
                    format!("{fwd}; fwd-status={fwd_status}{stored}; collapsed; ttl={ttl}");
                (resp, status, outcome)
            }
            Fetched::Failed {
                status,
                message,
                detail,
            } => (
                text(status, message),
                format!("{fwd}; collapsed; detail={detail}"),
                Outcome::Error,
            ),
            Fetched::Object { .. }
            | Fetched::Single(_)
            | Fetched::Streaming { .. }
            | Fetched::Sliced { .. } => return None,
        };
        self.shared.mark(&mut resp, &status, outcome);
        Some(resp)
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
    fn upstream_response(&mut self, resp: Response<Body>) -> BoxFuture<'_, Upstream> {
        Box::pin(async move {
            let (mut parts, body) = resp.into_parts();
            self.shared.pass_on(&mut parts.headers);
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
            Upstream::Respond(Response::from_parts(parts, body))
        })
    }

    fn deliver(self: Box<Self>, mut resp: Response<Body>, from: Delivery) -> Response<Body> {
        let (status, outcome) = match from {
            Delivery::Upstream => (self.status, Outcome::Pass),
            Delivery::Generated(reason) => {
                (format!("{}; detail={reason}", self.fwd), Outcome::Error)
            }
        };
        self.shared.mark(&mut resp, &status, outcome);
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
    /// The headers the request phase sent upstream, for a resend.
    forwarded: HeaderMap,
    req_cc: CacheControl,
    /// The stored copy being revalidated, if any.
    stored: Option<Arc<CachedObject>>,
    /// Sent again without validators after a `304` for another
    /// representation.
    resent: bool,
    /// The purge epoch before the request went upstream.
    epoch: u64,
    request_time: SystemTime,
    fwd: &'static str,
    /// `Cache-Status` for an upstream response.
    status: String,
    outcome: Outcome,
    /// Why the upstream response counts as an error, if it does.
    failure: Option<String>,
    /// The flight this fill leads, if followers wait on it.
    lead: Option<Lead<Fetched>>,
    /// The manifest whose missing slices this fill fetches, if it does.
    manifest: Option<Arc<CachedObject>>,
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

    /// Hands the result to the requests waiting on this fill.
    fn publish(&mut self, fetched: Fetched) {
        if let Some(lead) = self.lead.take() {
            lead.publish(fetched);
        }
    }

    /// A response that goes to the client as the upstream sent it.
    fn through(&mut self, mut parts: http::response::Parts, body: Body) -> Upstream {
        self.publish(Fetched::Single(parts.status));
        self.shared.pass_on(&mut parts.headers);
        self.status = format!("{}; fwd-status={}", self.fwd, parts.status.as_u16());
        self.outcome = Outcome::Miss;
        Upstream::Respond(Response::from_parts(parts, body))
    }

    /// An upstream response the layer could not use, answered with an
    /// error of its own.
    fn failed(&mut self, status: StatusCode, message: &'static str, detail: &str) -> Upstream {
        self.failure = Some(format!("detail-origin={detail}"));
        self.status = format!("{}; detail={detail}", self.fwd);
        self.outcome = Outcome::Error;
        self.publish(Fetched::Failed {
            status,
            message,
            detail: detail.to_owned(),
        });
        Upstream::Respond(text(status, message))
    }

    /// A stored object the client gets from the store.
    fn stored_object(&mut self, obj: Arc<CachedObject>, revalidated: bool) -> Upstream {
        let kept = self.shared.store(&self.key, &obj, self);
        let fwd_status = if revalidated {
            304
        } else {
            obj.status.as_u16()
        };
        self.status = format!(
            "{}; fwd-status={fwd_status}{}; ttl={}",
            self.fwd,
            if kept { "; stored" } else { "" },
            obj.freshness.ttl_secs(self.shared.clock.now())
        );
        self.outcome = if revalidated {
            Outcome::Revalidated
        } else {
            Outcome::Miss
        };
        let resp = self.serve(&obj);
        self.publish(Fetched::Object {
            obj,
            stored: kept,
            revalidated,
        });
        Upstream::Respond(resp)
    }

    fn not_modified(&mut self, stored: &CachedObject, parts: &http::response::Parts) -> Upstream {
        if !policy::not_modified_matches(&stored.headers, &parts.headers) {
            // The 304 is about another representation than the one asked
            // about, so it cannot refresh the copy: fetch in full.
            self.resent = true;
            let mut headers = self.forwarded.clone();
            headers.remove(header::IF_NONE_MATCH);
            headers.remove(header::IF_MODIFIED_SINCE);
            return Upstream::Resend(headers);
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
        self.stored_object(obj, true)
    }

    async fn fill(&mut self, resp: Response<Body>) -> Upstream {
        let (mut parts, body) = resp.into_parts();
        strip_hop_by_hop(&mut parts.headers);
        if parts.status == StatusCode::NOT_MODIFIED
            && !self.resent
            && let Some(stored) = self.stored.clone()
        {
            return self.not_modified(&stored, &parts);
        }
        if parts.status == StatusCode::PARTIAL_CONTENT
            && self.policy.slice_bytes > 0
            && let Some(manifest) = self.manifest.clone()
        {
            return self.slice_range_fill(parts, body, &manifest).await;
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
        if let Err(reason) = policy::storable(&exchange, self.policy.heuristic_max) {
            self.shared.observer.not_stored(reason);
            // The origin answered in full but no longer lets this be
            // stored. Drop the old copy too, or stale-if-error could keep
            // serving what the origin has since made private.
            if self.stored.is_some() && !self.resent && !is_error_status(parts.status) {
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
        if let Some(total) = declared
            && total > limit as u64
            && self.policy.slice_bytes > 0
            && parts.status == StatusCode::OK
        {
            let layout = Layout {
                slice_bytes: self.policy.slice_bytes,
                total,
            };
            return self.slice_fill(parts, body, layout, 0, total - 1);
        }
        if declared.is_some_and(|n| n > limit as u64) {
            self.shared.observer.streamed_uncached();
            return self.through(parts, body);
        }
        if let Some(len) = declared
            && len > STREAM_ABOVE
            && !needs_whole(&self.client_headers)
        {
            return self.stream(parts, body, len, &resp_cc);
        }
        let read = tokio::time::timeout(self.shared.body_timeout, read_limited(body, limit)).await;
        let bytes = match read {
            Ok(Ok(Read::Complete(bytes))) => bytes,
            Ok(Ok(Read::TooLarge(prefix, rest))) => {
                self.shared.observer.streamed_uncached();
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
        self.stored_object(obj, false)
    }
}

impl Fill {
    /// Answers the client with the body as it arrives, hands the same
    /// growing body to every follower, and stores the object once it is
    /// complete. The origin failing partway, sending more or fewer bytes
    /// than it declared, or taking longer than the body timeout aborts
    /// every reader, and nothing is stored.
    fn stream(
        &mut self,
        mut parts: http::response::Parts,
        body: Body,
        len: u64,
        resp_cc: &CacheControl,
    ) -> Upstream {
        parts.headers.remove(header::CONTENT_LENGTH);
        let freshness = self.freshness(parts.status, &parts.headers, resp_cc);
        let head = Arc::new(CachedObject::new(
            parts.status,
            parts.headers,
            Bytes::new(),
            freshness,
            &self.client_headers,
        ));
        let fill = Filling::new(Some(len));
        let now = self.shared.clock.now();
        self.status = format!(
            "{}; fwd-status={}; stored; ttl={}",
            self.fwd,
            head.status.as_u16(),
            head.freshness.ttl_secs(now)
        );
        self.outcome = Outcome::Miss;
        let resp = streamed(&head, &fill, len, now);
        // The flight stays open until the body is in, so a client that
        // arrives meanwhile joins this fill instead of starting another.
        let held = self.lead.take().map(|lead| {
            lead.publish_held(Fetched::Streaming {
                head: head.clone(),
                fill: fill.clone(),
            })
        });
        let shared = self.shared.clone();
        let key = self.key.clone();
        let client_headers = self.client_headers.clone();
        let (epoch, max_variants) = (self.epoch, self.policy.max_variants);
        tokio::spawn(async move {
            let feed = async {
                let mut body = body;
                let mut got = 0u64;
                while let Some(frame) = body.frame().await {
                    let Ok(frame) = frame else {
                        return Err("protocol");
                    };
                    if let Ok(data) = frame.into_data() {
                        got += data.len() as u64;
                        if got > len {
                            return Err("protocol");
                        }
                        fill.push(data);
                    }
                }
                if got == len { Ok(()) } else { Err("protocol") }
            };
            match tokio::time::timeout(shared.body_timeout, feed).await {
                Ok(Ok(())) => {
                    let obj = Arc::new(CachedObject::with_vary(
                        head.status,
                        head.headers.clone(),
                        fill.collect(),
                        head.freshness,
                        head.vary().clone(),
                    ));
                    let kept = shared.cache.store_limited(
                        key,
                        obj.clone(),
                        &client_headers,
                        epoch,
                        max_variants,
                    );
                    fill.finish(kept.then_some(obj));
                }
                Ok(Err(why)) => fill.abort(why),
                Err(_) => fill.abort("timeout"),
            }
            drop(held);
        });
        Upstream::Respond(resp)
    }
}

impl Fill {
    /// A `206` to the aligned Range this fill sent for slices a manifest
    /// lacked. Its entity tag and length are checked against the manifest:
    /// a different object at the origin drops the old manifest and slices
    /// first. Anything unusable goes to the client as it is.
    async fn slice_range_fill(
        &mut self,
        parts: http::response::Parts,
        body: Body,
        manifest: &CachedObject,
    ) -> Upstream {
        let Some(old) = Layout::of(manifest) else {
            return self.through(parts, body);
        };
        let range = parts
            .headers
            .get(header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range);
        let Some((first_byte, last_byte, total)) = range else {
            return self.through(parts, body);
        };
        let etag = crate::headers::EntityTag::from_header(&parts.headers);
        let same = total == old.total
            && match (&etag, &manifest.etag) {
                (Some(a), Some(b)) => a.strong_eq(b),
                _ => false,
            };
        let layout = if same {
            old
        } else {
            // The origin's object changed: none of the old slices may be
            // mixed with these.
            self.shared
                .cache
                .purge(&[PurgeTarget::Key(self.key.clone())])
                .await;
            Layout {
                slice_bytes: self.policy.slice_bytes,
                total,
            }
        };
        if first_byte % layout.slice_bytes != 0 || last_byte >= total {
            return self.through(parts, body);
        }
        self.slice_fill(
            parts,
            body,
            layout,
            first_byte / layout.slice_bytes,
            last_byte,
        )
    }

    /// Stores the response's bytes from slice `first` to byte `last_byte`
    /// as slices while every client reads them, then the manifest.
    fn slice_fill(
        &mut self,
        mut parts: http::response::Parts,
        body: Body,
        layout: Layout,
        first: u64,
        last_byte: u64,
    ) -> Upstream {
        let upstream_status = parts.status.as_u16();
        parts.headers.remove(header::CONTENT_LENGTH);
        parts.headers.remove(header::CONTENT_RANGE);
        let cc = CacheControl::from_headers(&parts.headers);
        let freshness = self.freshness(StatusCode::OK, &parts.headers, &cc);
        let mut manifest_headers = parts.headers.clone();
        manifest_headers.insert(SLICES, layout.header());
        let head = Arc::new(CachedObject::new(
            StatusCode::OK,
            manifest_headers,
            Bytes::new(),
            freshness,
            &self.client_headers,
        ));
        let progress = Progress::new(layout, first, last_byte);
        let now = self.shared.clock.now();
        let resp = match slice_range(&self.client_headers, head.etag.as_ref(), layout.total) {
            Err(resp) => *resp,
            Ok((status, start, end)) if progress.covers(start, end) || layout.total == 0 => {
                let body = (layout.total > 0).then(|| {
                    slice::read(
                        self.shared.cache.clone(),
                        self.key.clone(),
                        layout,
                        start,
                        end,
                        head.etag.clone(),
                        Some(progress.clone()),
                    )
                    .boxed()
                });
                sliced_response(&head, status, start, end, layout.total, body, now)
            }
            // Bytes this fill does not fetch: the slices it covers are still
            // stored, and the client is answered from what the origin sent.
            Ok(_) => {
                let resp = text(StatusCode::BAD_GATEWAY, "origin response invalid\n");
                return self.failed_with(resp, "range");
            }
        };
        self.status = format!(
            "{}; fwd-status={upstream_status}; stored; ttl={}",
            self.fwd,
            head.freshness.ttl_secs(now)
        );
        self.outcome = Outcome::Miss;
        let held = self.lead.take().map(|lead| {
            lead.publish_held(Fetched::Sliced {
                head: head.clone(),
                progress: progress.clone(),
            })
        });
        let shared = self.shared.clone();
        let key = self.key.clone();
        let client_headers = self.client_headers.clone();
        let (epoch, max_variants) = (self.epoch, self.policy.max_variants);
        let slice_headers = slice::slice_headers(&parts.headers, &key);
        tokio::spawn(async move {
            let store_slice = |index: u64, bytes: Bytes| {
                let obj = Arc::new(CachedObject::new(
                    StatusCode::OK,
                    slice_headers.clone(),
                    bytes,
                    freshness,
                    &HeaderMap::new(),
                ));
                shared.cache.store_limited(
                    slice::slice_key(&key, index),
                    obj,
                    &HeaderMap::new(),
                    epoch,
                    1,
                );
            };
            let expected = last_byte + 1 - layout.start_of(first);
            let feed = async {
                let mut body = body;
                let mut slicer = Slicer::new(layout.slice_bytes);
                let mut index = first;
                let mut got = 0u64;
                let mut fill = Filling::new(Some(layout.len_of(index)));
                progress.begin(index, fill.clone());
                while let Some(frame) = body.frame().await {
                    let Ok(frame) = frame else {
                        return Err("protocol");
                    };
                    let Ok(data) = frame.into_data() else {
                        continue;
                    };
                    got += data.len() as u64;
                    if got > expected {
                        return Err("protocol");
                    }
                    for (part, done) in slicer.push(data) {
                        fill.push(part);
                        if !done.is_empty() {
                            fill.finish(None);
                            store_slice(index, done.clone());
                            progress.stored(index, done);
                            index += 1;
                            if layout.start_of(index) <= last_byte {
                                fill = Filling::new(Some(layout.len_of(index)));
                                progress.begin(index, fill.clone());
                            }
                        }
                    }
                }
                if got != expected {
                    return Err("protocol");
                }
                if let Some(rest) = slicer.finish() {
                    fill.finish(None);
                    store_slice(index, rest.clone());
                    progress.stored(index, rest);
                }
                Ok(())
            };
            match tokio::time::timeout(shared.body_timeout, feed).await {
                Ok(Ok(())) => {
                    shared
                        .cache
                        .store_limited(key, head, &client_headers, epoch, max_variants);
                    progress.end(End::Complete(None));
                }
                Ok(Err(why)) => progress.end(End::Aborted(why.into())),
                Err(_) => progress.end(End::Aborted("timeout".into())),
            }
            drop(held);
        });
        Upstream::Respond(resp)
    }

    fn failed_with(&mut self, resp: Response<Body>, detail: &str) -> Upstream {
        self.failure = Some(format!("detail-origin={detail}"));
        self.status = format!("{}; detail={detail}", self.fwd);
        self.outcome = Outcome::Error;
        Upstream::Respond(resp)
    }
}

/// Parses `bytes <first>-<last>/<total>`.
fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    let (a, b, total) = (a.parse().ok()?, b.parse().ok()?, total.parse().ok()?);
    (a <= b && b < total).then_some((a, b, total))
}

impl Exchange for Fill {
    fn upstream_response(&mut self, resp: Response<Body>) -> BoxFuture<'_, Upstream> {
        Box::pin(self.fill(resp))
    }

    fn deliver(mut self: Box<Self>, mut resp: Response<Body>, from: Delivery) -> Response<Body> {
        let failure = match from {
            Delivery::Upstream => self.failure.clone(),
            // A response the server wrote counts as a failure only when it
            // is one of the upstream errors stale-if-error covers, not, say,
            // a request it refused.
            Delivery::Generated(reason) if is_error_status(resp.status()) => {
                self.publish(Fetched::Failed {
                    status: resp.status(),
                    message: failure_message(reason),
                    detail: reason.to_owned(),
                });
                Some(format!("detail-origin={reason}"))
            }
            // Followers go upstream themselves rather than share an answer
            // about this request alone.
            Delivery::Generated(_) => {
                self.lead = None;
                None
            }
        };
        if let Some(failure) = &failure
            && let Some(stale) = self.shared.stale_on_error(
                self.stored.as_ref(),
                &self.req_cc,
                &self.client_headers,
                failure,
            )
        {
            return stale;
        }
        let (status, outcome) = match from {
            Delivery::Upstream => (std::mem::take(&mut self.status), self.outcome),
            Delivery::Generated(reason) => {
                (format!("{}; detail={reason}", self.fwd), Outcome::Error)
            }
        };
        self.shared.mark(&mut resp, &status, outcome);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_failures_keep_the_message_for_their_reason() {
        assert_eq!(failure_message("connect"), "origin unreachable\n");
        assert_eq!(failure_message("timeout"), "origin timed out\n");
        assert_eq!(failure_message("protocol"), "origin response invalid\n");
        assert_eq!(
            failure_message("body-too-large"),
            "request body too large\n"
        );
        assert_eq!(failure_message("bad_gateway"), "upstream failed\n");
    }
}
