//! The request path: look up, serve, fetch with collapsing, store, and pass
//! through what cannot be cached.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime};

use bytes::{Bytes, BytesMut};
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::uri::PathAndQuery;
use http::{Method, Request, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Limited};
use hyper::body::{Body as HttpBody, Frame, Incoming, SizeHint};

use crate::cache_control::CacheControl;
use crate::collapse::{Collapser, Role};
use crate::headers::strip_hop_by_hop;
use crate::ids::{REQUEST_ID, request_id};
use crate::metrics::{Metrics, Outcome};
use crate::object::{CachedObject, Key, SURROGATE_KEY, primary_key};
use crate::origin::{OriginError, OriginPool, Outbound};
use crate::policy::{self, Exchange, Freshness, Use};
use crate::purge::PurgeTarget;
use crate::respond::{self, Body, BoxError, text};
use crate::store::Cache;

/// Wall-clock time, replaceable in tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
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

/// One origin response, as shared between collapsed requests.
#[derive(Clone)]
pub(crate) enum Fetched {
    /// A storable response, buffered. Every waiter may use it.
    Shared {
        obj: Arc<CachedObject>,
        stored: bool,
        /// Refreshed by a 304 rather than a full response.
        revalidated: bool,
    },
    /// A response only the leader may use: not storable (it may be
    /// personal), or too large to buffer.
    Single {
        status: StatusCode,
        response: Arc<Mutex<Option<Response<Body>>>>,
    },
    Failed(OriginError),
}

/// What was done for a request, for Cache-Status and the logs.
struct Answer {
    response: Response<Body>,
    outcome: Outcome,
    status: String,
}

pub struct Proxy {
    pub cache: Cache,
    pub pool: Arc<OriginPool>,
    pub metrics: Arc<Metrics>,
    collapser: Arc<Collapser<Fetched>>,
    clock: Arc<dyn Clock>,
    settings: ProxySettings,
    via: HeaderValue,
    cache_name: String,
}

/// Request headers that only make sense for the client that sent them, so
/// they are not forwarded on a shared fetch.
const CLIENT_CONDITIONALS: [HeaderName; 6] = [
    header::IF_NONE_MATCH,
    header::IF_MODIFIED_SINCE,
    header::IF_MATCH,
    header::IF_UNMODIFIED_SINCE,
    header::IF_RANGE,
    header::RANGE,
];

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
        Arc::new(Proxy {
            cache,
            pool,
            metrics: Arc::new(Metrics::default()),
            collapser: Arc::new(Collapser::default()),
            clock,
            settings,
            via,
            cache_name,
        })
    }

    /// Origin fetches currently shared through collapsing.
    pub fn in_flight(&self) -> usize {
        self.collapser.in_flight()
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
        let answer = self.dispatch(req, &id, client).await;
        let mut response = answer.response;
        let cache_status = format!("{}; {}", self.cache_name, answer.status);
        if let Ok(v) = HeaderValue::from_str(&cache_status) {
            response
                .headers_mut()
                .insert(HeaderName::from_static("cache-status"), v);
        }
        response.headers_mut().insert(REQUEST_ID, id.clone());
        let elapsed = started.elapsed();
        self.metrics.request(answer.outcome, elapsed);
        tracing::info!(
            request_id = id.to_str().unwrap_or(""),
            client = %client,
            method = %method,
            uri = %uri,
            status = response.status().as_u16(),
            outcome = answer.outcome.as_str(),
            cache_status = %answer.status,
            elapsed_us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
            "request"
        );
        response
    }

    async fn dispatch(
        self: &Arc<Self>,
        req: Request<Body>,
        id: &HeaderValue,
        client: SocketAddr,
    ) -> Answer {
        if self.looped(req.headers()) {
            return error(
                StatusCode::LOOP_DETECTED,
                "request loop detected\n",
                "detail=loop",
            );
        }
        if req.uri().to_string().len() > self.settings.max_uri_bytes {
            return error(
                StatusCode::URI_TOO_LONG,
                "request target too long\n",
                "detail=uri-too-long",
            );
        }
        let Some(key) = primary_key(req.uri(), req.headers()) else {
            return error(StatusCode::BAD_REQUEST, "missing host\n", "detail=no-host");
        };
        let req_cc = CacheControl::from_headers(req.headers());
        match *req.method() {
            Method::GET if !req_cc.no_store => self.cacheable(req, key, req_cc, id, client).await,
            Method::HEAD if !req_cc.no_store => self.head(req, key, req_cc, id, client).await,
            Method::GET | Method::HEAD => self.pass(req, key, id, client, "fwd=request").await,
            _ => self.pass(req, key, id, client, "fwd=method").await,
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

    /// Headers for a request to the origin.
    fn outbound_headers(
        &self,
        headers: &HeaderMap,
        id: &HeaderValue,
        client: SocketAddr,
        shared: bool,
    ) -> HeaderMap {
        let mut out = headers.clone();
        strip_hop_by_hop(&mut out);
        if shared {
            for name in &CLIENT_CONDITIONALS {
                out.remove(name);
            }
        }
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

    fn serve(&self, obj: &CachedObject, method: &Method, req: &HeaderMap) -> Response<Body> {
        respond::from_cache(obj, method, req, self.clock.now())
    }

    async fn head(
        self: &Arc<Self>,
        req: Request<Body>,
        key: Key,
        req_cc: CacheControl,
        id: &HeaderValue,
        client: SocketAddr,
    ) -> Answer {
        let now = self.clock.now();
        if let Some(obj) = self.cache.lookup(&key, req.headers()).await
            && matches!(
                policy::decide(&obj.freshness, &obj.cc, &req_cc, now),
                Use::Fresh | Use::StaleAccepted
            )
        {
            let status = format!("hit; ttl={}", obj.freshness.ttl_secs(now));
            return Answer {
                response: self.serve(&obj, &Method::HEAD, req.headers()),
                outcome: Outcome::Hit,
                status,
            };
        }
        self.pass(req, key, id, client, "fwd=miss").await
    }

    async fn cacheable(
        self: &Arc<Self>,
        req: Request<Body>,
        key: Key,
        req_cc: CacheControl,
        id: &HeaderValue,
        client: SocketAddr,
    ) -> Answer {
        let (parts, _) = req.into_parts();
        let now = self.clock.now();
        let mut fetch = FetchRequest {
            key: key.clone(),
            path: path_of(&parts.uri),
            headers: parts.headers.clone(),
            outbound: self.outbound_headers(&parts.headers, id, client, true),
            req_cc,
            stored: None,
        };
        if let Some(obj) = self.cache.lookup(&key, &parts.headers).await {
            let ttl = obj.freshness.ttl_secs(now);
            match policy::decide(&obj.freshness, &obj.cc, &fetch.req_cc, now) {
                Use::Fresh => {
                    return Answer {
                        response: self.serve(&obj, &Method::GET, &parts.headers),
                        outcome: Outcome::Hit,
                        status: format!("hit; ttl={ttl}"),
                    };
                }
                Use::StaleAccepted => {
                    return Answer {
                        response: self.serve(&obj, &Method::GET, &parts.headers),
                        outcome: Outcome::Stale,
                        status: format!("hit; ttl={ttl}; detail=max-stale"),
                    };
                }
                Use::StaleWhileRevalidate => {
                    let mut refresh = fetch.clone();
                    refresh.stored = Some(obj.clone());
                    self.refresh_in_background(refresh);
                    return Answer {
                        response: self.serve(&obj, &Method::GET, &parts.headers),
                        outcome: Outcome::Stale,
                        status: format!("hit; ttl={ttl}; detail=stale-while-revalidate"),
                    };
                }
                Use::Revalidate => fetch.stored = Some(obj),
            }
        }
        if fetch.req_cc.only_if_cached {
            return error(
                StatusCode::GATEWAY_TIMEOUT,
                "not in cache\n",
                "fwd=miss; detail=only-if-cached",
            );
        }
        let proxy = self.clone();
        let flight = fetch.clone();
        let (result, role) = self
            .collapser
            .run(key, move || async move { proxy.fetch(flight).await })
            .await;
        let fetched = result.unwrap_or(Fetched::Failed(OriginError::Protocol));
        if role == Role::Follower {
            self.metrics.collapsed.fetch_add(1, Ordering::Relaxed);
        }
        self.answer_fetch(fetched, role, &fetch).await
    }

    /// Starts a collapsed refresh that nobody waits for.
    fn refresh_in_background(self: &Arc<Self>, fetch: FetchRequest) {
        let proxy = self.clone();
        let key = fetch.key.clone();
        if self
            .collapser
            .spawn(key, move || async move { proxy.fetch(fetch).await })
        {
            self.metrics
                .background_refreshes
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Why this request went forward: no stored copy, a stale one, or a
    /// fresh one the request's own directives would not accept.
    fn forward_reason(&self, fetch: &FetchRequest) -> &'static str {
        match &fetch.stored {
            None => "fwd=miss",
            Some(obj) if obj.freshness.is_fresh(self.clock.now()) => "fwd=request",
            Some(_) => "fwd=stale",
        }
    }

    /// Serves the stored copy in place of an origin failure when
    /// stale-if-error allows it (RFC 5861 section 4).
    fn stale_on_error(&self, fetch: &FetchRequest, origin_status: &str) -> Option<Answer> {
        let obj = fetch.stored.as_ref()?;
        let now = self.clock.now();
        if !policy::stale_if_error_allows(&obj.freshness, &obj.cc, &fetch.req_cc, now) {
            return None;
        }
        Some(Answer {
            response: self.serve(obj, &Method::GET, &fetch.headers),
            outcome: Outcome::StaleIfError,
            status: format!(
                "hit; ttl={}; fwd=stale; {origin_status}; detail=stale-if-error",
                obj.freshness.ttl_secs(now)
            ),
        })
    }

    /// Turns a fetch result into this request's answer. Followers that
    /// cannot use the shared result fetch on their own.
    async fn answer_fetch(
        self: &Arc<Self>,
        fetched: Fetched,
        role: Role,
        fetch: &FetchRequest,
    ) -> Answer {
        let collapsed = if role == Role::Follower {
            "; collapsed"
        } else {
            ""
        };
        let fwd = self.forward_reason(fetch);
        let failure = match &fetched {
            Fetched::Failed(e) => Some(format!("detail-origin={}", e.as_str())),
            Fetched::Shared { obj, .. } if is_error_status(obj.status) => {
                Some(format!("fwd-status={}", obj.status.as_u16()))
            }
            Fetched::Single { status, .. } if is_error_status(*status) => {
                Some(format!("fwd-status={}", status.as_u16()))
            }
            _ => None,
        };
        if let Some(origin_status) = failure
            && let Some(answer) = self.stale_on_error(fetch, &origin_status)
        {
            return answer;
        }
        match fetched {
            Fetched::Shared {
                obj,
                stored,
                revalidated,
            } if obj.matches(&fetch.headers) => {
                let now = self.clock.now();
                let stored = if stored { "; stored" } else { "" };
                let (outcome, fwd_status) = if revalidated {
                    (Outcome::Revalidated, 304)
                } else {
                    (Outcome::Miss, obj.status.as_u16())
                };
                Answer {
                    response: self.serve(&obj, &Method::GET, &fetch.headers),
                    outcome,
                    status: format!(
                        "{fwd}; fwd-status={fwd_status}{stored}{collapsed}; ttl={}",
                        obj.freshness.ttl_secs(now)
                    ),
                }
            }
            Fetched::Single { status, response } if role == Role::Leader => {
                let taken = response
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                match taken {
                    Some(response) => Answer {
                        response,
                        outcome: Outcome::Miss,
                        status: format!("{fwd}; fwd-status={}", status.as_u16()),
                    },
                    None => self.fetch_alone(fetch).await,
                }
            }
            Fetched::Failed(e) => origin_error(e, &format!("{fwd}{collapsed}")),
            // A follower cannot use a response meant for one client, or a
            // variant that does not match its own request.
            Fetched::Shared { .. } | Fetched::Single { .. } => self.fetch_alone(fetch).await,
        }
    }

    /// Fetches without collapsing, for a follower that could not share.
    async fn fetch_alone(self: &Arc<Self>, fetch: &FetchRequest) -> Answer {
        let fetched = self.clone().fetch(fetch.clone()).await;
        Box::pin(self.answer_fetch(fetched, Role::Leader, fetch)).await
    }

    /// Sends one request to the origin, conditional when a stored copy
    /// exists, and stores the response if allowed.
    async fn fetch(self: Arc<Self>, fetch: FetchRequest) -> Fetched {
        let epoch = self.cache.epoch();
        let request_time = self.clock.now();
        let mut headers = fetch.outbound.clone();
        if let Some(stored) = &fetch.stored {
            for (name, value) in policy::conditional_headers(&stored.headers) {
                headers.insert(name, value);
            }
        }
        let out = Outbound {
            method: Method::GET,
            path: fetch.path.clone(),
            headers,
            body: None,
        };
        let (resp, _origin) = match self.pool.send(out).await {
            Ok(r) => r,
            Err(e) => return Fetched::Failed(e),
        };
        let response_time = self.clock.now();
        let (mut parts, body) = resp.into_parts();
        strip_hop_by_hop(&mut parts.headers);
        if parts.status == StatusCode::NOT_MODIFIED
            && let Some(stored) = &fetch.stored
        {
            if !policy::not_modified_matches(&stored.headers, &parts.headers) {
                // The 304 is about some other representation; start over.
                let mut fresh = fetch.clone();
                fresh.stored = None;
                return Box::pin(self.fetch(fresh)).await;
            }
            let headers = policy::merge_not_modified(&stored.headers, &parts.headers);
            let cc = CacheControl::from_headers(&headers);
            let freshness = Freshness::compute(
                stored.status,
                &headers,
                &cc,
                request_time,
                response_time,
                self.settings.heuristic_max,
            );
            let obj = Arc::new(CachedObject::with_vary(
                stored.status,
                headers,
                stored.body.clone(),
                freshness,
                stored.vary().clone(),
            ));
            let stored = self
                .cache
                .store(fetch.key.clone(), obj.clone(), &fetch.headers, epoch);
            return Fetched::Shared {
                obj,
                stored,
                revalidated: true,
            };
        }
        let resp_cc = CacheControl::from_headers(&parts.headers);
        let exchange = Exchange {
            method: &Method::GET,
            req_headers: &fetch.headers,
            req_cc: &fetch.req_cc,
            status: parts.status,
            resp_headers: &parts.headers,
            resp_cc: &resp_cc,
            response_time,
        };
        if let Err(reason) = policy::storable(&exchange, self.settings.heuristic_max) {
            self.metrics.not_stored(reason);
            return self.single(parts, body.map_err(Into::into).boxed());
        }
        let limit = self.cache.memory().max_object_bytes();
        let declared = parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
        if declared.is_some_and(|n| n > limit as u64) {
            self.metrics
                .streamed_uncached
                .fetch_add(1, Ordering::Relaxed);
            return self.single(parts, body.map_err(Into::into).boxed());
        }
        let read =
            tokio::time::timeout(self.settings.body_timeout, read_limited(body, limit)).await;
        let bytes = match read {
            Ok(Ok(Read::Complete(bytes))) => bytes,
            Ok(Ok(Read::TooLarge(prefix, rest))) => {
                self.metrics
                    .streamed_uncached
                    .fetch_add(1, Ordering::Relaxed);
                let body = Prefixed {
                    prefix: Some(prefix),
                    rest,
                }
                .map_err(Into::into)
                .boxed();
                return self.single(parts, body);
            }
            Ok(Err(_)) => return Fetched::Failed(OriginError::Protocol),
            Err(_) => return Fetched::Failed(OriginError::Timeout),
        };
        // The body is complete now; framing headers describe the stored copy.
        parts.headers.remove(header::CONTENT_LENGTH);
        let freshness = Freshness::compute(
            parts.status,
            &parts.headers,
            &resp_cc,
            request_time,
            response_time,
            self.settings.heuristic_max,
        );
        let obj = Arc::new(CachedObject::new(
            parts.status,
            parts.headers,
            bytes,
            freshness,
            &fetch.headers,
        ));
        let stored = self
            .cache
            .store(fetch.key.clone(), obj.clone(), &fetch.headers, epoch);
        Fetched::Shared {
            obj,
            stored,
            revalidated: false,
        }
    }

    fn single(&self, mut parts: http::response::Parts, body: Body) -> Fetched {
        parts.headers.remove(SURROGATE_KEY);
        parts.headers.append(header::VIA, self.via.clone());
        let status = parts.status;
        let response = Response::from_parts(parts, body);
        Fetched::Single {
            status,
            response: Arc::new(Mutex::new(Some(response))),
        }
    }

    /// Forwards a request the cache does not handle. Unsafe methods that
    /// succeed invalidate the URLs they name (RFC 9111 section 4.4).
    async fn pass(
        self: &Arc<Self>,
        req: Request<Body>,
        key: Key,
        id: &HeaderValue,
        client: SocketAddr,
        status: &str,
    ) -> Answer {
        let (parts, body) = req.into_parts();
        let declared = parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
        if declared.is_some_and(|n| n > self.settings.max_request_body) {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large\n",
                "detail=body-too-large",
            );
        }
        let has_body = !(parts.method == Method::GET || parts.method == Method::HEAD)
            || declared.is_some_and(|n| n > 0)
            || parts.headers.contains_key(header::TRANSFER_ENCODING);
        let limit = usize::try_from(self.settings.max_request_body).unwrap_or(usize::MAX);
        let out = Outbound {
            method: parts.method.clone(),
            path: path_of(&parts.uri),
            headers: self.outbound_headers(&parts.headers, id, client, false),
            body: has_body.then(|| Limited::new(body, limit).boxed()),
        };
        let (resp, _origin) = match self.pool.send(out).await {
            Ok(r) => r,
            Err(e) => return origin_error(e, status),
        };
        let (mut rparts, rbody) = resp.into_parts();
        strip_hop_by_hop(&mut rparts.headers);
        rparts.headers.remove(SURROGATE_KEY);
        rparts.headers.append(header::VIA, self.via.clone());
        let unsafe_method = !matches!(
            parts.method,
            Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
        );
        if unsafe_method && (rparts.status.is_success() || rparts.status.is_redirection()) {
            let mut targets = vec![PurgeTarget::Key(key.clone())];
            for name in [header::LOCATION, header::CONTENT_LOCATION] {
                if let Some(other) =
                    same_host_key(&key, &parts.uri, &parts.headers, rparts.headers.get(&name))
                {
                    targets.push(PurgeTarget::Key(other));
                }
            }
            self.cache.purge(&targets).await;
        }
        let fwd_status = rparts.status.as_u16();
        Answer {
            response: Response::from_parts(rparts, rbody.map_err(Into::into).boxed()),
            outcome: Outcome::Pass,
            status: format!("{status}; fwd-status={fwd_status}"),
        }
    }
}

#[derive(Clone)]
struct FetchRequest {
    key: Key,
    path: PathAndQuery,
    /// The client's request headers, for Vary and storability.
    headers: HeaderMap,
    /// Headers to send to the origin.
    outbound: HeaderMap,
    req_cc: CacheControl,
    /// The stored copy being revalidated, if any.
    stored: Option<Arc<CachedObject>>,
}

/// Origin statuses that count as errors for stale-if-error (RFC 5861 section 4).
fn is_error_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 500 | 502 | 503 | 504)
}

fn path_of(uri: &Uri) -> PathAndQuery {
    uri.path_and_query()
        .cloned()
        .unwrap_or_else(|| PathAndQuery::from_static("/"))
}

/// The cache key for a Location or Content-Location value on the same host.
fn same_host_key(
    key: &Key,
    uri: &Uri,
    headers: &HeaderMap,
    value: Option<&HeaderValue>,
) -> Option<Key> {
    let target: Uri = value?.to_str().ok()?.parse().ok()?;
    let own = primary_key(uri, headers)?;
    let host = &own[..own.len() - path_of(uri).as_str().len()];
    let other = match target.authority() {
        Some(a) if a.as_str().eq_ignore_ascii_case(host) => format!("{host}{}", path_of(&target)),
        Some(_) => return None,
        None if target.path().starts_with('/') => format!("{host}{}", path_of(&target)),
        None => return None,
    };
    (other.as_str() != &**key).then(|| other.into())
}

fn error(status: StatusCode, message: &'static str, detail: &str) -> Answer {
    Answer {
        response: text(status, message),
        outcome: Outcome::Error,
        status: detail.to_string(),
    }
}

fn origin_error(e: OriginError, fwd: &str) -> Answer {
    let (status, message, detail) = match e {
        OriginError::Connect => (StatusCode::BAD_GATEWAY, "origin unreachable\n", "connect"),
        OriginError::Timeout => (StatusCode::GATEWAY_TIMEOUT, "origin timed out\n", "timeout"),
        OriginError::Protocol => (
            StatusCode::BAD_GATEWAY,
            "origin response invalid\n",
            "protocol",
        ),
        OriginError::RequestBody => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body too large\n",
            "body-too-large",
        ),
    };
    error(status, message, &format!("{fwd}; detail={detail}"))
}

enum Read {
    Complete(Bytes),
    TooLarge(Bytes, Incoming),
}

/// Reads a body up to `limit` bytes. Past the limit, hands back what was
/// read and the rest of the stream instead of buffering more.
async fn read_limited(mut body: Incoming, limit: usize) -> Result<Read, hyper::Error> {
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
struct Prefixed {
    prefix: Option<Bytes>,
    rest: Incoming,
}

impl HttpBody for Prefixed {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        if let Some(prefix) = self.prefix.take() {
            return Poll::Ready(Some(Ok(Frame::data(prefix))));
        }
        Pin::new(&mut self.rest).poll_frame(cx)
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = self.rest.size_hint();
        if let Some(p) = &self.prefix {
            let n = p.len() as u64;
            hint.set_lower(hint.lower() + n);
            if let Some(upper) = hint.upper() {
                hint.set_upper(upper + n);
            }
        }
        hint
    }
}

impl crate::server::Handler for Proxy {
    async fn call(self: Arc<Self>, req: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
        self.handle(req, peer).await
    }
}
