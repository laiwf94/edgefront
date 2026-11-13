//! The HTTP/1.1 request path: steer's `internal/proxy` handler, served by
//! [`edgefront_core::listener`] and forwarding through
//! [`edgefront_core::http1::Client`].
//!
//! Each request goes through the same steps as in steer, which put Go's
//! `net/http` server in front of `httputil.ReverseProxy`:
//!
//! 1. **Go's server checks.** A target `url.ParseRequestURI` refuses, more
//!    than one `Host` header, a missing `Host` on HTTP/1.1 or a `Host` with
//!    a byte `httpguts.ValidHostHeader` refuses get Go's own `400` (plain
//!    text, `Connection: close`) before the handler runs, and `OPTIONS *`
//!    gets Go's empty `200`.
//! 2. **The handler.** `CONNECT` gets `405 method_not_allowed`. The request
//!    is routed on its host and cleaned path; no match is `404 no_route`.
//!    The cluster's `max_requests` is enforced, refusing with
//!    `503 overloaded` and `X-Steer-Overflow: cluster_max_requests`.
//! 3. **The pick.** The cluster's [`Balancer`] picks the endpoint: a
//!    priority level from the share of available endpoints in each, with
//!    the panic threshold, then an endpoint inside it by the cluster's
//!    policy. Maglev hashes the first value of the cluster's `hash_header`
//!    when the request has a non-empty one. No pick is
//!    `503 no_healthy_upstream`. The endpoint counts the request in flight
//!    until its response body is done, and observes the time to the
//!    response head on the balancer's clock (at least a second when the
//!    attempt failed or answered 5xx, as steer penalised failures).
//! 4. **The upstream request.** Hop-by-hop headers and the headers named in
//!    `Connection` are removed (`TE: trailers` survives), the forwarding
//!    headers are rewritten (see [`crate::forwarded`]), the request ID is
//!    set, and the target is the one [`RequestTarget::upstream_target`]
//!    gives. The body keeps the client's framing: chunked stays chunked,
//!    and a length is sent as it came (zero only for `POST`, `PUT` and
//!    `PATCH`, as Go writes it). With retries configured, a body of at most
//!    `max_body_bytes` is read before the upstream is contacted, as steer
//!    buffered the bodies it might replay; a failure reading it is
//!    `400 bad_request`.
//! 5. **The response.** Hop-by-hop headers are removed and the request ID
//!    is added; the body streams. The cluster's `timeout` covers the whole
//!    exchange: before the response head it is `504 upstream_timeout`,
//!    after it the response is cut. An upstream that cannot be reached or
//!    breaks before answering is `502 bad_gateway`.
//!
//! A route can carry a filter ([`edgefront_core::filter`], attached with
//! [`Proxy::with_filters`]), such as the cache. Its request phase runs
//! after routing and before `max_requests`, so an answer it gives, such as
//! a cache hit, takes no place under the limit and never reaches an
//! upstream. Its upstream response phase sees the head and body before
//! the request ID is added, and its deliver phase sees every response,
//! including the proxy's own errors, named by their steer code. A
//! background refresh a filter asks for takes a place under
//! `max_requests` and the cluster's `timeout` like any other request.
//!
//! Errors the proxy writes itself have steer's shape,
//! `{"error":{"code":...,"message":...,"request_id":...}}`, and steer's
//! headers. Each request is logged once, as steer's access log was.
//!
//! What steer did on top of this arrives with W03: health checks, outlier
//! detection, retries with their budget and per-try timeout, the admin API
//! and metrics. Until then every endpoint stays healthy and admitted unless
//! something drives its state through [`Proxy::endpoints`].

use std::future::Future as _;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use edgefront_core::body::{self, Body, BoxError};
use edgefront_core::clock::{Clock, MonotonicClock};
use edgefront_core::config::{Cluster as ClusterConfig, Config, IpPrefix};
use edgefront_core::filter::{Decision, Delivery, Exchange, RequestHead, RouteFilters};
use edgefront_core::http1::{Client, ClientError, ClientLimits};
use edgefront_core::listener::Handler;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, Request, Response, StatusCode, Uri, Version};
use http_body_util::BodyExt;
use hyper::body::{Body as HttpBody, Frame, Incoming, SizeHint};
use tokio::time::{Instant, Sleep};

use crate::balancer::maglev::hash_key;
use crate::balancer::rng::{Rng, SplitMix64};
use crate::balancer::{Balancer, Options};
use crate::endpoint::{Endpoint, InFlight};
use crate::forwarded;
use crate::path::RequestTarget;
use crate::reqid;
use crate::route::Router;

/// The header naming the limit that refused a request with `503`.
pub const OVERFLOW_HEADER: HeaderName = HeaderName::from_static("x-steer-overflow");

/// The least latency a failed attempt is recorded with, steer's
/// `failurePenalty`: without it an endpoint that refuses connections or
/// answers 503 at once would look fastest to the least-loaded policy.
pub const FAILURE_PENALTY: Duration = Duration::from_secs(1);

/// The methods steer's `405` lists in `Allow`.
const ALLOW: &str = "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS";

/// The request headers that only concern one hop, which `httputil`
/// removes in both directions (RFC 9110 section 7.6.1, plus the obsolete
/// `Proxy-Connection` and `Keep-Alive`).
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "proxy-connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The proxy for one configuration. It is a [`Handler`] for
/// [`edgefront_core::listener::serve`].
pub struct Proxy {
    /// Each route's index and its cluster's.
    router: Router<(usize, usize)>,
    clusters: Vec<Arc<Cluster>>,
    trusted: Vec<IpPrefix>,
    /// The layers on each route, such as the cache.
    filters: RouteFilters,
}

struct Cluster {
    name: String,
    balancer: Balancer,
    /// The header Maglev hashes, when the policy is Maglev.
    hash_header: Option<HeaderName>,
    clock: Arc<dyn Clock>,
    client: Client<Body>,
    timeout: Option<Duration>,
    in_flight: Arc<AtomicU32>,
    max_requests: u32,
    /// The largest body buffered for replay, when retries are on.
    replay_limit: Option<u64>,
}

impl Cluster {
    fn new(cfg: &ClusterConfig, clock: &Arc<dyn Clock>, rng: &Arc<dyn Rng>) -> Self {
        let epoch = clock.now();
        let endpoints: Vec<Endpoint> = cfg
            .endpoints
            .iter()
            .map(|e| Endpoint::new(&e.address, e.weight, e.priority.into(), epoch))
            .collect();
        let opts = Options::new(cfg.policy)
            .panic_threshold(cfg.panic_threshold_percent)
            .clock(Arc::clone(clock))
            .rng(Arc::clone(rng));
        // Validation keeps every weight in 1..=1000, every priority at most
        // 7 and at most 1,024 endpoints, below Maglev's default table size.
        let balancer = Balancer::new(endpoints, opts).expect("a validated cluster");
        let limits = ClientLimits {
            connect_timeout: cfg.connect_timeout,
            // The cluster timeout bounds the whole exchange instead.
            response_header_timeout: None,
            ..ClientLimits::default()
        };
        Cluster {
            name: cfg.name.clone(),
            balancer,
            hash_header: cfg
                .hash_header
                .as_deref()
                .and_then(|h| HeaderName::from_bytes(h.as_bytes()).ok()),
            clock: Arc::clone(clock),
            client: Client::new(&limits),
            timeout: Some(cfg.timeout).filter(|t| !t.is_zero()),
            in_flight: Arc::new(AtomicU32::new(0)),
            max_requests: cfg.max_requests,
            replay_limit: cfg
                .retry
                .as_ref()
                .filter(|r| r.max_retries > 0)
                .map(|r| r.max_body_bytes),
        }
    }

    /// The request's hash key: steer's `HashKey` of the first value of the
    /// hash header, when there is a non-empty one.
    fn key(&self, headers: &HeaderMap) -> Option<u64> {
        let value = headers.get(self.hash_header.as_ref()?)?;
        (!value.is_empty()).then(|| hash_key(value.as_bytes()))
    }

    fn acquire(&self) -> Option<Permit> {
        let prev = self.in_flight.fetch_add(1, Ordering::AcqRel);
        if prev >= self.max_requests {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Permit(self.in_flight.clone()))
    }
}

/// One request's place under its cluster's `max_requests`, held until the
/// response body is done.
struct Permit(Arc<AtomicU32>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Proxy {
    /// Builds the proxy for a validated configuration, balancing on the
    /// monotonic clock with a generator seeded from entropy. Must be called
    /// inside a tokio runtime, which runs the clients' pools.
    pub fn new(cfg: &Config) -> Self {
        Proxy::with_sources(
            cfg,
            MonotonicClock::shared(),
            Arc::new(SplitMix64::from_entropy()),
        )
    }

    /// Builds the proxy with the clock its balancers read availability and
    /// latency on, and the generator every cluster's random draws come
    /// from. A manual clock and a seeded generator make the picks
    /// repeatable, which is how they are diffed against steer.
    pub fn with_sources(cfg: &Config, clock: Arc<dyn Clock>, rng: Arc<dyn Rng>) -> Self {
        let clusters: Vec<Arc<Cluster>> = cfg
            .clusters
            .iter()
            .map(|c| Arc::new(Cluster::new(c, &clock, &rng)))
            .collect();
        let index = |name: &str| {
            clusters
                .iter()
                .position(|c| c.name == name)
                .expect("validation ties every route to a cluster")
        };
        let router = Router::new(cfg.routes.iter().enumerate().map(|(ri, r)| {
            (
                r.host.as_str(),
                r.path_prefix.as_str(),
                (ri, index(&r.cluster)),
            )
        }));
        Proxy {
            router,
            clusters,
            trusted: cfg.trusted_proxies.clone(),
            filters: RouteFilters::new(cfg.routes.len()),
        }
    }

    /// Runs requests on each route through that route's filter in
    /// `filters`, indexed as the configuration's routes are. Routes with
    /// no filter are proxied as before, at the cost of one branch.
    #[must_use]
    pub fn with_filters(mut self, filters: RouteFilters) -> Self {
        self.filters = filters;
        self
    }

    /// The live endpoints of the cluster named `name`, in configuration
    /// order, or `None` when there is no such cluster. Their state is what
    /// the balancer reads on every pick.
    pub fn endpoints(&self, name: &str) -> Option<&[Endpoint]> {
        self.clusters
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.balancer.endpoints())
    }

    /// The balancer of the cluster named `name`.
    pub fn balancer(&self, name: &str) -> Option<&Balancer> {
        self.clusters
            .iter()
            .find(|c| c.name == name)
            .map(|c| &c.balancer)
    }

    /// Answers one request from `peer`.
    pub async fn handle(&self, req: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
        let started = Instant::now();
        let (mut parts, incoming) = req.into_parts();

        if parts.method == Method::CONNECT {
            let id = reqid::from_request(&parts.headers);
            let mut resp = error(
                &id,
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "CONNECT is not supported",
            );
            let h = resp.headers_mut();
            h.insert(header::ALLOW, HeaderValue::from_static(ALLOW));
            // Go's server closes a connection after a CONNECT it answered.
            h.insert(header::CONNECTION, HeaderValue::from_static("close"));
            log(
                &Access::new(&parts, &id, peer, &self.trusted, b"/"),
                started,
                &resp,
                "connect_method",
            );
            return resp;
        }

        let raw_target = parts.uri.path_and_query().map_or("/", |p| p.as_str());
        if parts.method == Method::OPTIONS && raw_target == "*" {
            // Go's server answers this itself (`globalOptionsHandler`).
            return Response::new(body::empty());
        }
        let target = match RequestTarget::parse(raw_target.as_bytes()) {
            Ok(t) => t,
            Err(_) => return go_bad_request(None),
        };
        let host = match request_host(&parts.uri, &parts.headers, parts.version) {
            Ok(h) => h.to_owned(),
            Err(reason) => return go_bad_request(reason),
        };

        let id = reqid::from_request(&parts.headers);
        let access = Access::new(&parts, &id, peer, &self.trusted, target.clean_path());
        let Some(&(ri, ci)) = self.router.route(&host, target.clean_path()) else {
            let resp = error(
                &id,
                StatusCode::NOT_FOUND,
                "no_route",
                "no route matches this host and path",
            );
            log(&access, started, &resp, "no_route");
            return resp;
        };
        let cluster = &self.clusters[ci];
        let access = access.cluster(&cluster.name);
        // The request phase runs before the cluster's `max_requests`, so
        // an answer from a filter, such as a cache hit, takes no place.
        let mut exchange = None;
        if let Some(filter) = self.filters.get(ri) {
            let decision = filter
                .request(RequestHead {
                    route: ri,
                    method: &parts.method,
                    uri: &parts.uri,
                    headers: &mut parts.headers,
                })
                .await;
            let resp = match decision {
                Decision::Forward(x) => {
                    exchange = x;
                    None
                }
                Decision::Respond(resp) => Some(resp),
                Decision::Refresh(resp, x) => {
                    let mut headers = parts.headers.clone();
                    self.upstream_headers(&mut headers, &parts.uri, access.peer, &host, &id);
                    let refresh = Refresh {
                        cluster: Arc::clone(cluster),
                        method: parts.method.clone(),
                        key: cluster.key(&parts.headers),
                        headers,
                        target: target.upstream_target().into_owned(),
                        id: id.clone(),
                        exchange: x,
                    };
                    tokio::spawn(refresh.run());
                    Some(resp)
                }
            };
            if let Some(mut resp) = resp {
                resp.headers_mut().insert(reqid::HEADER, id);
                log(&access, started, &resp, "");
                return resp;
            }
        }
        let Some(permit) = cluster.acquire() else {
            let mut resp = error(
                &id,
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded",
                "too many concurrent requests to this service",
            );
            resp.headers_mut().insert(
                OVERFLOW_HEADER,
                HeaderValue::from_static("cluster_max_requests"),
            );
            let resp = deliver_generated(exchange, resp, "overloaded", &id);
            log(&access, started, &resp, "overflow");
            return resp;
        };
        let key = cluster.key(&parts.headers);
        let mut headers = parts.headers;
        let length = request_length(&headers, &incoming);
        self.upstream_headers(&mut headers, &parts.uri, access.peer, &host, &id);

        let deadline = cluster.timeout.map(|t| started + t);
        let mut picked = None;
        let upstream_target = target.upstream_target();
        let exchange_fut = async {
            // steer reads a body it may replay before it picks.
            let Ok(body) = upstream_body(incoming, length, cluster.replay_limit).await else {
                return Err(Failure::ClientBody);
            };
            cluster
                .forward(
                    &parts.method,
                    headers,
                    &upstream_target,
                    body,
                    length,
                    key,
                    &mut picked,
                )
                .await
        };
        let result = match deadline {
            Some(at) => tokio::time::timeout_at(at, exchange_fut)
                .await
                .unwrap_or(Err(Failure::Timeout)),
            None => exchange_fut.await,
        };
        let access = match picked {
            Some(i) => access.endpoint(&cluster.balancer.endpoints()[i].address),
            None => access,
        };

        let failure = match result {
            Ok((resp, in_flight)) => {
                let mut resp = proxied(resp, deadline, permit, in_flight);
                if let Some(mut x) = exchange {
                    resp = x.upstream_response(resp).await;
                    resp = x.deliver(resp, Delivery::Upstream);
                }
                resp.headers_mut().insert(reqid::HEADER, id);
                log(&access, started, &resp, "");
                return resp;
            }
            Err(f) => f,
        };
        let (status, code, message, logged) = failure.describe();
        let resp = error(&id, status, code, message);
        let resp = deliver_generated(exchange, resp, code, &id);
        log(&access, started, &resp, logged);
        resp
    }

    /// Turns the client's request headers into the upstream's: hop-by-hop
    /// headers go, the forwarding headers are rewritten, and the request ID
    /// and, for an absolute-form target, the `Host` are set.
    fn upstream_headers(
        &self,
        headers: &mut HeaderMap,
        uri: &Uri,
        peer: Option<std::net::IpAddr>,
        host: &str,
        id: &HeaderValue,
    ) {
        strip_hop_by_hop(headers, true);
        forwarded::set_forwarded(headers, peer, false, host.as_bytes(), &self.trusted);
        headers.insert(reqid::HEADER, id.clone());
        if uri.authority().is_some() {
            // Go sends `Request.Host`, which an absolute-form target sets.
            headers.insert(
                header::HOST,
                HeaderValue::from_str(host).expect("an authority"),
            );
        }
    }
}

impl Cluster {
    /// Picks an endpoint and sends it the request, recording the pick in
    /// `picked` as soon as it is made, and the time to the response head
    /// on the endpoint.
    #[allow(clippy::too_many_arguments)]
    async fn forward(
        &self,
        method: &Method,
        mut headers: HeaderMap,
        target: &[u8],
        body: Body,
        length: Option<u64>,
        key: Option<u64>,
        picked: &mut Option<usize>,
    ) -> Result<(Response<Incoming>, InFlight), Failure> {
        let Some(i) = self.balancer.pick(key, &[]) else {
            return Err(Failure::NoHealthyUpstream);
        };
        *picked = Some(i);
        let endpoint = &self.balancer.endpoints()[i];
        let in_flight = endpoint.state.track();
        set_framing(&mut headers, method, length);
        let uri = format!(
            "http://{}{}",
            endpoint.address,
            String::from_utf8_lossy(target)
        );
        let mut out = Request::new(body);
        *out.method_mut() = method.clone();
        *out.uri_mut() = Uri::try_from(uri).map_err(|_| Failure::Proxy)?;
        *out.version_mut() = Version::HTTP_11;
        *out.headers_mut() = headers;
        let began = self.clock.now();
        let result = self.client.send(out).await;
        // A cancelled exchange (the cluster timeout) observes nothing,
        // as steer skipped requests whose context ended.
        let now = self.clock.now();
        let elapsed = now.saturating_duration_since(began);
        let sample = match &result {
            Ok(r) if !r.status().is_server_error() => elapsed,
            _ => elapsed.max(FAILURE_PENALTY),
        };
        endpoint.state.observe_latency(sample, now);
        match result {
            Ok(resp) => Ok((resp, in_flight)),
            Err(e) => Err(Failure::Upstream(e)),
        }
    }
}

/// An upstream response on its way to the client, with the hop-by-hop
/// headers removed and its body cut at the deadline.
fn proxied(
    resp: Response<Incoming>,
    deadline: Option<Instant>,
    permit: Permit,
    in_flight: InFlight,
) -> Response<Body> {
    let (mut head, incoming) = resp.into_parts();
    strip_hop_by_hop(&mut head.headers, false);
    // Go writes the standard reason phrase whatever the upstream sent.
    head.extensions.remove::<hyper::ext::ReasonPhrase>();
    let body = Proxied {
        inner: incoming,
        deadline: deadline.map(|at| Box::pin(tokio::time::sleep_until(at))),
        _permit: permit,
        _in_flight: in_flight,
    };
    Response::from_parts(head, BodyExt::boxed(body))
}

/// An error the proxy wrote, through the filter's deliver phase when the
/// request has an exchange.
fn deliver_generated(
    exchange: Option<Box<dyn Exchange>>,
    resp: Response<Body>,
    reason: &'static str,
    id: &HeaderValue,
) -> Response<Body> {
    let Some(x) = exchange else {
        return resp;
    };
    let mut resp = x.deliver(resp, Delivery::Generated(reason));
    resp.headers_mut().insert(reqid::HEADER, id.clone());
    resp
}

/// A request a filter answered at once and wants sent upstream in the
/// background (a cache refreshing a stale copy). It takes a place under
/// the cluster's `max_requests` and its timeout like any other request,
/// carries the client request's ID, and has no body. What the exchange
/// delivers is dropped.
struct Refresh {
    cluster: Arc<Cluster>,
    method: Method,
    key: Option<u64>,
    headers: HeaderMap,
    target: Vec<u8>,
    id: HeaderValue,
    exchange: Box<dyn Exchange>,
}

impl Refresh {
    async fn run(self) {
        let Refresh {
            cluster,
            method,
            key,
            headers,
            target,
            id,
            mut exchange,
        } = self;
        let Some(permit) = cluster.acquire() else {
            let resp = error(
                &id,
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded",
                "too many concurrent requests to this service",
            );
            drop(exchange.deliver(resp, Delivery::Generated("overloaded")));
            return;
        };
        let deadline = cluster.timeout.map(|t| Instant::now() + t);
        let mut picked = None;
        let send = cluster.forward(
            &method,
            headers,
            &target,
            body::empty(),
            Some(0),
            key,
            &mut picked,
        );
        let result = match deadline {
            Some(at) => tokio::time::timeout_at(at, send)
                .await
                .unwrap_or(Err(Failure::Timeout)),
            None => send.await,
        };
        match result {
            Ok((resp, in_flight)) => {
                let resp = proxied(resp, deadline, permit, in_flight);
                let resp = exchange.upstream_response(resp).await;
                drop(exchange.deliver(resp, Delivery::Upstream));
            }
            Err(f) => {
                let (status, code, message, _) = f.describe();
                let resp = error(&id, status, code, message);
                drop(exchange.deliver(resp, Delivery::Generated(code)));
            }
        }
    }
}

impl Handler for Proxy {
    type Body = Body;

    async fn call(self: Arc<Self>, req: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
        self.handle(req, peer).await
    }
}

/// Why the proxy answered instead of an upstream.
enum Failure {
    NoHealthyUpstream,
    Timeout,
    ClientBody,
    Upstream(ClientError),
    Proxy,
}

impl Failure {
    /// The status, steer's error code and message, and the access log's
    /// `failure` field.
    fn describe(&self) -> (StatusCode, &'static str, &'static str, &'static str) {
        match self {
            Failure::NoHealthyUpstream => (
                StatusCode::SERVICE_UNAVAILABLE,
                "no_healthy_upstream",
                "no healthy upstream is available",
                "no_healthy_upstream",
            ),
            Failure::Timeout => (
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "the upstream did not answer in time",
                "request_timeout",
            ),
            Failure::ClientBody => (
                StatusCode::BAD_REQUEST,
                "bad_request",
                "could not read the request body",
                "client_body",
            ),
            Failure::Upstream(e) => (
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                "the upstream failed to answer",
                match e {
                    ClientError::Connect | ClientError::ConnectTimeout => "connect_error",
                    ClientError::Timeout => "timeout",
                    ClientError::HeadersTooLarge | ClientError::Protocol => "reset",
                },
            ),
            Failure::Proxy => (
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                "the upstream failed to answer",
                "proxy_error",
            ),
        }
    }
}

/// The host a request is routed on (Go's `Request.Host`), after the checks
/// Go's server makes before any handler runs. `Err` holds the text Go adds
/// to its `400`, if any.
fn request_host<'a>(
    uri: &'a Uri,
    headers: &'a HeaderMap,
    version: Version,
) -> Result<&'a str, Option<&'static str>> {
    let mut hosts = headers.get_all(header::HOST).iter();
    let first = hosts.next();
    if hosts.next().is_some() {
        return Err(None); // "too many Host headers", answered as a plain 400
    }
    let Some(value) = first else {
        if version >= Version::HTTP_11 {
            return Err(Some("missing required Host header"));
        }
        return Ok(uri.authority().map_or("", |a| a.as_str()));
    };
    if !value.as_bytes().iter().all(|&c| valid_host_byte(c)) {
        return Err(Some("malformed Host header"));
    }
    // An absolute-form target names the host itself.
    Ok(match uri.authority() {
        Some(a) => a.as_str(),
        None => value.to_str().expect("checked to be ASCII"),
    })
}

/// Go's `httpguts.ValidHostHeader` byte set: letters, digits and
/// `!$%&'()*+,-.:;=[]_~`.
fn valid_host_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!$%&'()*+,-.:;=[]_~".contains(&c)
}

/// Go's answer to a request its server refused: `400 Bad Request`, with
/// the reason in the status line and body when Go gives one, as plain text
/// on a connection that then closes.
fn go_bad_request(reason: Option<&'static str>) -> Response<Body> {
    let text = match reason {
        Some(r) => format!("400 Bad Request: {r}"),
        None => "400 Bad Request".to_owned(),
    };
    let mut resp = Response::new(body::full(text));
    *resp.status_mut() = StatusCode::BAD_REQUEST;
    if let Some(r) = reason {
        let phrase = format!("Bad Request: {r}");
        resp.extensions_mut().insert(
            hyper::ext::ReasonPhrase::try_from(phrase.into_bytes()).expect("a static ASCII phrase"),
        );
    }
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    h.insert(header::CONNECTION, HeaderValue::from_static("close"));
    resp
}

/// One of steer's JSON errors.
fn error(id: &HeaderValue, status: StatusCode, code: &str, message: &str) -> Response<Body> {
    // The ID passed `reqid::valid` or is hex, and the code and message are
    // fixed, so nothing needs JSON escaping. Go's encoder ends with "\n".
    let id_text = id.to_str().expect("request IDs are ASCII");
    let text = format!(
        "{{\"error\":{{\"code\":\"{code}\",\"message\":\"{message}\",\"request_id\":\"{id_text}\"}}}}\n"
    );
    let mut resp = Response::new(body::full(text));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(reqid::HEADER, id.clone());
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

/// Removes hop-by-hop headers and the headers `Connection` names, as
/// `httputil.ReverseProxy` does. For a request, `TE: trailers` is kept
/// when the client offered it.
fn strip_hop_by_hop(headers: &mut HeaderMap, request: bool) {
    let trailers = request
        && headers
            .get_all(header::TE)
            .iter()
            .any(|v| has_token(v.as_bytes(), b"trailers"));
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .flat_map(|v| v.as_bytes().split(|&c| c == b','))
        .filter_map(|t| HeaderName::from_bytes(t.trim_ascii()).ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
    if trailers {
        headers.insert(header::TE, HeaderValue::from_static("trailers"));
    }
}

/// Whether a comma-separated header value holds `token`, ignoring case and
/// parameters (Go's `httpguts.HeaderValuesContainsToken`).
fn has_token(value: &[u8], token: &[u8]) -> bool {
    value.split(|&c| c == b',').any(|t| {
        let t = t.split(|&c| c == b';').next().unwrap_or_default();
        t.trim_ascii().eq_ignore_ascii_case(token)
    })
}

/// The request body's length as Go's server saw it: `Some(n)` for a
/// `Content-Length` body (hyper has checked it), `None` when chunked.
fn request_length(headers: &HeaderMap, body: &Incoming) -> Option<u64> {
    if headers.contains_key(header::TRANSFER_ENCODING) {
        return None;
    }
    Some(body.size_hint().exact().unwrap_or(0))
}

/// Frames the upstream body the way Go's transport did for steer, which
/// kept the client's framing: a chunked body goes chunked (even when it
/// was buffered), and a body with a length goes with that length, where
/// zero is only written for methods that carry a body.
fn set_framing(headers: &mut HeaderMap, method: &Method, length: Option<u64>) {
    headers.remove(header::CONTENT_LENGTH);
    match length {
        Some(0) if !matches!(*method, Method::POST | Method::PUT | Method::PATCH) => {}
        Some(n) => {
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(n));
        }
        None => {
            headers.insert(
                header::TRANSFER_ENCODING,
                HeaderValue::from_static("chunked"),
            );
        }
    }
}

/// The body to send upstream. With a replay limit, a body of at most that
/// many bytes is read first, as steer buffered the bodies it might retry;
/// a longer one is sent as read so far followed by the rest. `Err` means
/// the client's body failed.
async fn upstream_body(
    mut incoming: Incoming,
    length: Option<u64>,
    replay_limit: Option<u64>,
) -> Result<Body, ()> {
    if length == Some(0) {
        return Ok(body::empty());
    }
    let Some(limit) = replay_limit.filter(|&l| length.is_none_or(|n| n <= l)) else {
        return Ok(incoming.map_err(BoxError::from).boxed());
    };
    let mut buf = BytesMut::new();
    while (buf.len() as u64) <= limit {
        match incoming.frame().await {
            None => return Ok(body::full(buf.freeze())),
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    buf.extend_from_slice(&data);
                }
            }
            Some(Err(_)) => return Err(()),
        }
    }
    let body = Prefixed {
        prefix: Some(buf.freeze()),
        rest: incoming,
    };
    Ok(BodyExt::boxed(body))
}

/// A body that was partly read: the bytes read, then the rest.
struct Prefixed {
    prefix: Option<Bytes>,
    rest: Incoming,
}

impl HttpBody for Prefixed {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if let Some(prefix) = self.prefix.take() {
            return Poll::Ready(Some(Ok(Frame::data(prefix))));
        }
        Pin::new(&mut self.rest)
            .poll_frame(cx)
            .map_err(BoxError::from)
    }

    fn is_end_stream(&self) -> bool {
        self.prefix.is_none() && self.rest.is_end_stream()
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

/// The upstream's response body on its way to the client: cut when the
/// cluster's timeout passes, and holding the request's place under
/// `max_requests` and in its endpoint's in-flight count until it is
/// dropped.
struct Proxied {
    inner: Incoming,
    deadline: Option<Pin<Box<Sleep>>>,
    _permit: Permit,
    _in_flight: InFlight,
}

/// The error that cuts a response body at the cluster's timeout.
#[derive(Debug)]
struct Expired;

impl std::fmt::Display for Expired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the cluster timeout passed while the response streamed")
    }
}

impl std::error::Error for Expired {}

impl HttpBody for Proxied {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if let Some(d) = self.deadline.as_mut()
            && d.as_mut().poll(cx).is_ready()
        {
            return Poll::Ready(Some(Err(Expired.into())));
        }
        Pin::new(&mut self.inner)
            .poll_frame(cx)
            .map_err(BoxError::from)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// What the access log line says about a request.
struct Access<'a> {
    id: String,
    peer: Option<std::net::IpAddr>,
    client: String,
    method: Method,
    host: String,
    path: String,
    cluster: &'a str,
    endpoint: &'a str,
}

impl<'a> Access<'a> {
    fn new(
        parts: &http::request::Parts,
        id: &HeaderValue,
        peer: SocketAddr,
        trusted: &[IpPrefix],
        path: &[u8],
    ) -> Self {
        let peer = forwarded::peer_addr(&peer.to_string());
        let xff = parts
            .headers
            .get_all(&forwarded::X_FORWARDED_FOR)
            .iter()
            .map(HeaderValue::as_bytes);
        let client = forwarded::client_ip(peer, xff, trusted);
        let host = parts.headers.get(header::HOST).map(HeaderValue::as_bytes);
        Access {
            id: id.to_str().unwrap_or_default().to_owned(),
            peer,
            client: client.map(|c| c.to_string()).unwrap_or_default(),
            method: parts.method.clone(),
            host: String::from_utf8_lossy(host.unwrap_or_default()).into_owned(),
            path: String::from_utf8_lossy(path).into_owned(),
            cluster: "",
            endpoint: "",
        }
    }

    fn cluster(self, cluster: &'a str) -> Self {
        Access { cluster, ..self }
    }

    fn endpoint(self, endpoint: &'a str) -> Self {
        Access { endpoint, ..self }
    }
}

/// Logs one request as steer's access log did: at info, or at warn for a
/// 5xx.
fn log<B>(a: &Access<'_>, started: Instant, resp: &Response<B>, failure: &str) {
    let status = resp.status().as_u16();
    let duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    macro_rules! line {
        ($level:ident) => {
            tracing::$level!(
                request_id = a.id,
                client_ip = a.client,
                method = a.method.as_str(),
                host = a.host,
                path = a.path,
                cluster = a.cluster,
                endpoint = a.endpoint,
                status,
                duration_ms,
                failure,
                "request"
            )
        };
    }
    if status >= 500 {
        line!(warn);
    } else {
        line!(info);
    }
}
