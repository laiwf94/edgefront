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
//! # Resilience
//!
//! A cluster with a `health_check` probes its endpoints and one with
//! `outlier_detection` ejects them, both into the endpoint state the
//! balancer reads. The probes and the detectors' ticks run in
//! [`Proxy::run_checks`], which the binary spawns beside the listener;
//! the consecutive-failure detector also runs on every attempt. A cluster
//! without them keeps every endpoint healthy and admitted.
//!
//! A cluster with `retry` runs steer's attempt loop (its
//! `transport.go`): every request deposits into the retry budget, each
//! attempt is held to the per-try timeout and classified (`success`,
//! `http_5xx`, `connect_error`, `timeout` or `reset`), reported to the
//! outlier detector and observed as latency, and a retryable outcome
//! picks another endpoint not tried yet, withdraws from the budget,
//! waits the full-jitter backoff and tries again. A connect error is
//! always retryable; a timeout, a reset or a `5xx` in `retry_on` only for
//! an idempotent request; and only a body that is empty or was buffered
//! whole can be sent again. When nothing more may be tried, the last
//! response is delivered, or the last failure answered: `504
//! upstream_timeout` for a timeout and `502 bad_gateway` otherwise. The
//! response of an attempt that is retried is dropped after its first
//! 4 KiB are read, as steer drained it.
//!
//! # Operating
//!
//! The configuration in force is a runtime that [`Proxy::reload`] swaps
//! for a new one, carrying endpoint state, in-flight counts, connection
//! pools and retry budgets over as steer's reload did; a request finishes
//! on the runtime it started with. Every step above records steer's
//! metric series ([`Proxy::metrics`]), [`Proxy::state`] is the live state
//! the admin API's `/clusters` reports, and [`Proxy::drain`] closes each
//! connection after its response while the process drains.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use bytes::{Bytes, BytesMut};
use edgefront_core::admin::{BalancerState, ClusterState, EndpointState, ProxyState};
use edgefront_core::body::{self, Body, BoxError};
use edgefront_core::clock::{Clock, MonotonicClock};
use edgefront_core::config::{Cluster as ClusterConfig, Config, IpPrefix, limits};
use edgefront_core::filter::{Decision, Delivery, Exchange, RequestHead, RouteFilters, Upstream};
use edgefront_core::http1::{ClientLimits, TlsTo};
use edgefront_core::listener::Handler;
use edgefront_core::reload;
use edgefront_core::telemetry::metrics::{
    ClusterMetrics, DiscoveryMetrics, Registry, SteerMetrics, TlsMetrics,
};
use edgefront_core::tls::client_auth::spiffe_matches;
use edgefront_core::tls::{ClientIdentity, TlsInfo};
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, Request, Response, StatusCode, Uri, Version};
use http_body_util::BodyExt;
use hyper::body::{Body as HttpBody, Frame, Incoming, SizeHint};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, Sleep};

use crate::balancer::lb::Patch;
use crate::balancer::maglev::hash_key;
use crate::balancer::rng::{Rng, SplitMix64};
use crate::balancer::{Balancer, Options};
use crate::discovery::{self as disc, Merger, Update, UpdateKind};
use crate::endpoint::{Endpoint, InFlight};
use crate::forwarded;
use crate::grpc;
use crate::health::{self, Checker};
use crate::outlier::Detector;
use crate::path::RequestTarget;
use crate::protocol::{ResponseBody, Upstream as Way};
use crate::reqid;
use crate::retry::{self, Budget, Outcome, Policy};
use crate::route::Router;
use edgefront_core::chunked::Chunked;

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

/// The proxy for one configuration at a time. It is a [`Handler`] for
/// [`edgefront_core::listener::serve`], and [`Proxy::reload`] swaps in a
/// new configuration while it serves.
pub struct Proxy {
    /// The configuration in force. A request takes the runtime it starts
    /// with and finishes on it, as steer's did; [`Proxy::run_checks`]
    /// follows the changes.
    current: watch::Sender<Arc<Runtime>>,
    clock: Arc<dyn Clock>,
    rng: Arc<dyn Rng>,
    metrics: Arc<SteerMetrics>,
    /// The version `steer_config_version` reads on each scrape.
    version: Arc<AtomicU64>,
    /// The layers on each route, such as the cache.
    filters: RouteFilters,
    /// Held while a reload builds and swaps a runtime.
    reloading: Mutex<()>,
    /// Set by [`Proxy::drain`].
    draining: AtomicBool,
    /// What each client connection may carry ([`Proxy::with_conn_quota`]).
    conn_quota: edgefront_core::listener::ConnQuota,
    /// Decides each new connection ([`Proxy::with_connection_gate`]).
    gate: Option<Arc<dyn Fn(SocketAddr) -> bool + Send + Sync>>,
    /// The latest endpoints discovery gave each cluster that has it, read
    /// whenever a runtime is built.
    /// A [`Chunked`] list, which each discovery change patches.
    discovered: Mutex<HashMap<String, Chunked<Arc<disc::Endpoint>>>>,
    /// The merged version each discovering cluster balances on.
    discovered_versions: Mutex<HashMap<String, u64>>,
}

/// One loaded configuration, never changed after it is built (steer's
/// `runtime`).
struct Runtime {
    version: u64,
    loaded_at: SystemTime,
    listen: String,
    admin_listen: String,
    routes: usize,
    /// Each route's index and its cluster's.
    router: Arc<Router<(usize, usize)>>,
    clusters: Vec<Arc<Cluster>>,
    trusted: Arc<[IpPrefix]>,
}

impl Runtime {
    /// The runtime for `cfg`, carrying state over from `old` by cluster
    /// name and endpoint address.
    fn build(cfg: &Config, old: Option<&Runtime>, version: u64, src: &Sources<'_>) -> Runtime {
        let clusters: Vec<Arc<Cluster>> = cfg
            .clusters
            .iter()
            .map(|c| {
                let prev = old.and_then(|o| o.clusters.iter().find(|p| p.config.name == c.name));
                Arc::new(Cluster::new(c, prev.map(|p| &**p), src))
            })
            .collect();
        let index = |name: &str| {
            clusters
                .iter()
                .position(|c| c.config.name == name)
                .expect("validation ties every route to a cluster")
        };
        let router = Arc::new(Router::new(cfg.routes.iter().enumerate().map(|(ri, r)| {
            (
                r.host.as_str(),
                r.path_prefix.as_str(),
                (ri, index(&r.cluster)),
            )
        })));
        Runtime {
            version,
            loaded_at: SystemTime::now(),
            listen: cfg.listen.clone(),
            admin_listen: cfg.admin_listen.clone(),
            routes: cfg.routes.len(),
            router,
            clusters,
            trusted: cfg.trusted_proxies.clone().into(),
        }
    }

    fn cluster(&self, name: &str) -> Option<&Arc<Cluster>> {
        self.clusters.iter().find(|c| c.config.name == name)
    }

    /// The same runtime with the cluster at `index` replaced, as discovery
    /// does: same version, routes and every other cluster.
    fn with_cluster(&self, index: usize, cluster: Arc<Cluster>) -> Runtime {
        let mut clusters = self.clusters.clone();
        clusters[index] = cluster;
        Runtime {
            version: self.version,
            loaded_at: self.loaded_at,
            listen: self.listen.clone(),
            admin_listen: self.admin_listen.clone(),
            routes: self.routes,
            router: Arc::clone(&self.router),
            clusters,
            trusted: Arc::clone(&self.trusted),
        }
    }
}

/// What every cluster is built with.
struct Sources<'a> {
    clock: &'a Arc<dyn Clock>,
    rng: &'a Arc<dyn Rng>,
    metrics: &'a Arc<SteerMetrics>,
    /// Each discovering cluster's latest endpoints.
    discovered: &'a HashMap<String, Chunked<Arc<disc::Endpoint>>>,
}

struct Cluster {
    config: ClusterConfig,
    /// `None` while the cluster has no endpoints, which only discovery
    /// gives it: every request then fails with no healthy upstream.
    balancer: Option<Balancer>,
    /// The header Maglev hashes, when the policy is Maglev.
    hash_header: Option<HeaderName>,
    clock: Arc<dyn Clock>,
    /// The way to the endpoints: HTTP/1.1, HTTP/2 or either.
    client: Arc<Way>,
    timeout: Option<Duration>,
    /// Requests holding a permit; shared with the cluster of the same name
    /// in the next runtime, so a reload keeps the count right.
    in_flight: Arc<AtomicU32>,
    max_requests: u32,
    /// The retry policy and budget, when the cluster retries.
    retry: Option<Retrying>,
    /// Outlier detection, when configured.
    detector: Option<Arc<Detector>>,
    /// Active health checks, when configured.
    checker: Option<Arc<Checker>>,
    /// Each endpoint's index in the balancer, by address, for
    /// [`Cluster::patched`]. The next cluster patched from this one takes
    /// it; `None` until a patch first needs it, or once taken.
    index: Mutex<Option<HashMap<Arc<str>, usize>>>,
    /// The draws behind each retry's backoff.
    jitter: SplitMix64,
    /// steer's per-cluster series.
    metrics: ClusterMetrics,
}

/// An endpoint a cluster is to balance over.
struct Wanted<'a> {
    address: &'a str,
    weight: u32,
    priority: u32,
}

/// The ready endpoints of a discovered set.
fn ready<'a>(found: impl IntoIterator<Item = &'a disc::Endpoint>) -> Vec<Wanted<'a>> {
    found
        .into_iter()
        .filter(|e| e.ready)
        .map(|e| Wanted {
            address: &e.address,
            weight: e.weight,
            priority: e.priority.into(),
        })
        .collect()
}

/// The endpoints for `wanted`, each keeping the state of the endpoint of
/// the same address in `prev`. An endpoint nothing will probe is made
/// healthy again, and one nothing will eject is let back in.
fn carry(
    wanted: &[Wanted<'_>],
    prev: Option<&Chunked<Endpoint>>,
    epoch: std::time::Instant,
    cfg: &ClusterConfig,
) -> Vec<Endpoint> {
    let old: HashMap<&str, &Arc<crate::endpoint::State>> = prev
        .into_iter()
        .flatten()
        .map(|e| (&*e.address, &e.state))
        .collect();
    wanted
        .iter()
        .map(|w| {
            let ep = match old.get(w.address) {
                Some(state) => {
                    Endpoint::with_state(w.address, w.weight, w.priority, Arc::clone(state))
                }
                None => Endpoint::new(w.address, w.weight, w.priority, epoch),
            };
            if cfg.health_check.is_none() && !ep.state.healthy() {
                ep.state.set_healthy(true);
            }
            if cfg.outlier_detection.is_none() {
                ep.state.clear_ejection();
            }
            ep
        })
        .collect()
}

/// The balancer options of `cfg`.
fn options(cfg: &ClusterConfig, src: &Sources<'_>) -> Options {
    Options::new(cfg.policy)
        .panic_threshold(cfg.panic_threshold_percent)
        .clock(Arc::clone(src.clock))
        .rng(Arc::clone(src.rng))
}

/// A cluster's retry settings and the budget its retries draw on.
struct Retrying {
    policy: Policy,
    /// Shared with the next runtime when a reload keeps the budget's
    /// settings.
    budget: Arc<Budget>,
}

impl Cluster {
    /// The cluster for `cfg`. With `prev`, the cluster of the same name in
    /// the runtime being replaced, it carries on as steer's reload did:
    /// endpoints that keep their address keep their health, ejection and
    /// latency state; the in-flight count is shared; the connection pool
    /// is kept while `connect_timeout` is unchanged; and the retry budget
    /// is kept while its settings are. An endpoint nothing will probe is
    /// made healthy again, and one nothing will eject is let back in.
    ///
    /// A cluster with discovery balances over the ready endpoints its
    /// sources last gave, or over its listed endpoints until they first
    /// answer.
    fn new(cfg: &ClusterConfig, prev: Option<&Cluster>, src: &Sources<'_>) -> Self {
        let clock = src.clock;
        let epoch = clock.now();
        let listed = || -> Vec<Wanted<'_>> {
            cfg.endpoints
                .iter()
                .map(|e| Wanted {
                    address: &e.address,
                    weight: e.weight,
                    priority: e.priority.into(),
                })
                .collect()
        };
        let wanted = match (&cfg.discovery, src.discovered.get(&cfg.name)) {
            (Some(_), Some(found)) => ready(found.iter().map(|e| &**e)),
            _ => listed(),
        };
        let endpoints = carry(&wanted, prev.map(Cluster::endpoints), epoch, cfg);
        let opts = options(cfg, src);
        // Validation keeps every weight in 1..=1000, every priority at most
        // 7 and at most 1,024 listed endpoints, below Maglev's default
        // table size; discovery checks its sets before it publishes them.
        let balancer = (!endpoints.is_empty())
            .then(|| Balancer::new(endpoints, opts))
            .and_then(|b| {
                b.map_err(|e| {
                    tracing::error!(cluster = cfg.name, error = %e, "cannot balance");
                })
                .ok()
            });
        let eps = || {
            balancer
                .as_ref()
                .map(|b| b.endpoints().clone())
                .unwrap_or_default()
        };
        let retry = cfg.retry.as_ref().map(|r| {
            let kept = prev.and_then(|p| match (&p.config.retry, &p.retry) {
                (Some(old), Some(rt)) if old.budget == r.budget => Some(Arc::clone(&rt.budget)),
                _ => None,
            });
            Retrying {
                policy: Policy::new(r),
                budget: kept.unwrap_or_else(|| Arc::new(Budget::new(&r.budget, Arc::clone(clock)))),
            }
        });
        let name = cfg.name.clone();
        let detector = cfg.outlier_detection.as_ref().map(|od| {
            let cluster = name.clone();
            let timer = Arc::clone(clock);
            let metrics = Arc::clone(src.metrics);
            let on_eject: crate::outlier::EjectFn = Box::new(move |ep, reason, until| {
                metrics.outlier_ejection(&cluster, reason);
                let ejected_for_ms = until.saturating_duration_since(timer.now()).as_millis();
                tracing::warn!(
                    cluster,
                    endpoint = &*ep.address,
                    reason,
                    ejected_for_ms,
                    "endpoint ejected"
                );
            });
            Arc::new(Detector::new(
                od.clone(),
                eps(),
                Arc::clone(clock),
                Some(on_eject),
            ))
        });
        // TLS to the endpoints, read afresh on every build, so a reload
        // picks up rotated CA bundles and client certificates. A block
        // that cannot be read here was readable when the file was loaded;
        // its cluster then refuses every attempt rather than speak
        // cleartext.
        let tls = cfg.tls.as_ref().map(|t| {
            TlsTo::load(t).map_err(|e| {
                tracing::error!(
                    cluster = name,
                    error = e.as_str(),
                    "cannot load upstream tls"
                );
            })
        });
        let checker = cfg.health_check.as_ref().map(|hc| {
            let cluster = name.clone();
            let metrics = Arc::clone(src.metrics);
            let on_change: health::ChangeFn = Box::new(move |ep, healthy, reason| {
                metrics.health_transition(&cluster, healthy);
                if healthy {
                    tracing::info!(cluster, endpoint = &*ep.address, "endpoint healthy");
                } else {
                    tracing::warn!(
                        cluster,
                        endpoint = &*ep.address,
                        reason,
                        "endpoint unhealthy"
                    );
                }
            });
            let checker = Checker::new(hc.clone(), eps(), Arc::clone(clock), Some(on_change));
            Arc::new(match &tls {
                Some(Ok(t)) => checker.with_tls(t.clone()),
                _ => checker,
            })
        });
        let limits = ClientLimits {
            connect_timeout: cfg.connect_timeout,
            // The cluster timeout bounds the whole exchange instead.
            response_header_timeout: None,
            ..ClientLimits::default()
        };
        let client = match (&tls, prev) {
            (Some(Ok(t)), _) => Way::new(cfg.protocol, &limits, Some(t)),
            (Some(Err(())), _) => Way::refusing(&limits),
            (None, Some(p))
                if p.config.connect_timeout == cfg.connect_timeout
                    && p.config.tls.is_none()
                    && p.config.protocol == cfg.protocol =>
            {
                Way::with_http1(cfg.protocol, &limits, None, p.client.http1().clone())
            }
            (None, _) => Way::new(cfg.protocol, &limits, None),
        };
        let list = eps();
        let addresses: Vec<&str> = list.iter().map(|e| &*e.address).collect();
        Cluster {
            config: cfg.clone(),
            hash_header: cfg
                .hash_header
                .as_deref()
                .and_then(|h| HeaderName::from_bytes(h.as_bytes()).ok()),
            clock: Arc::clone(clock),
            client: Arc::new(client),
            timeout: Some(cfg.timeout).filter(|t| !t.is_zero()),
            in_flight: prev
                .map_or_else(|| Arc::new(AtomicU32::new(0)), |p| Arc::clone(&p.in_flight)),
            max_requests: cfg.max_requests,
            retry,
            detector,
            checker,
            jitter: SplitMix64::from_entropy(),
            metrics: src.metrics.cluster(&cfg.name, &addresses),
            index: Mutex::new(None),
            balancer,
        }
    }

    /// The endpoints the cluster balances over, in order.
    fn endpoints(&self) -> &Chunked<Endpoint> {
        static NONE: std::sync::LazyLock<Chunked<Endpoint>> =
            std::sync::LazyLock::new(Chunked::new);
        self.balancer.as_ref().map_or(&NONE, Balancer::endpoints)
    }

    /// Each endpoint's index by address: the one this cluster keeps, or
    /// one built from its endpoints when another patch took it.
    fn take_index(&self) -> HashMap<Arc<str>, usize> {
        let kept = self
            .index
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        kept.unwrap_or_else(|| {
            self.endpoints()
                .iter()
                .enumerate()
                .map(|(i, e)| (Arc::clone(&e.address), i))
                .collect()
        })
    }

    /// The same cluster after discovery changed the endpoints at the
    /// addresses of `touched`, each with its merged endpoint now (ready or
    /// not), or `None` once no source names it. Only those endpoints are
    /// looked at: a ready endpoint not balanced over yet joins at the end,
    /// one that went or is no longer ready is removed by swap, and one
    /// whose weight or priority changed is replaced in place with its
    /// state. The balancer, the attempt series and the checker's and
    /// detector's lists are patched by the same changes, sharing every
    /// chunk of the old lists they leave alone, so a change costs its own
    /// size rather than the cluster's (Maglev alone builds its table
    /// again). Everything else carries on as in [`Cluster::rediscovered`].
    /// Refused, and this cluster left as it was, when the balancer cannot
    /// take the result.
    fn patched(
        &self,
        touched: &[(&str, Option<&disc::Endpoint>)],
        src: &Sources<'_>,
    ) -> Result<Cluster, String> {
        let cfg = &self.config;
        let mut index = self.take_index();
        let mut patch = match &self.balancer {
            Some(b) => b.patch(),
            None => Patch::new(options(cfg, src)),
        };
        let mut attempts = self.metrics.attempt_series().clone();
        let mut probes: Vec<(bool, Endpoint)> = Vec::new();
        let mut gone: Vec<Arc<str>> = Vec::new();
        let mut joined: Vec<Arc<str>> = Vec::new();
        let epoch = src.clock.now();
        for &(address, found) in touched {
            let wanted = found.filter(|e| e.ready);
            match (index.get(address).copied(), wanted) {
                (None, None) => {}
                (Some(i), None) => {
                    let old = patch.swap_remove(i);
                    attempts.swap_remove(i);
                    index.remove(address);
                    if let Some(moved) = patch.endpoints().get(i) {
                        index.insert(Arc::clone(&moved.address), i);
                    }
                    gone.push(Arc::clone(&old.address));
                    probes.push((false, old));
                }
                (Some(i), Some(e)) => {
                    let old = &patch.endpoints()[i];
                    if old.weight != e.weight || old.priority != u32::from(e.priority) {
                        let ep = Endpoint {
                            weight: e.weight,
                            priority: e.priority.into(),
                            ..old.clone()
                        };
                        patch.set(i, ep);
                    }
                }
                (None, Some(e)) => {
                    let ep = Endpoint::new(&e.address, e.weight, e.priority.into(), epoch);
                    index.insert(Arc::clone(&ep.address), patch.len());
                    attempts.push(src.metrics.endpoint_attempts(&cfg.name, &e.address));
                    joined.push(Arc::clone(&ep.address));
                    probes.push((true, ep.clone()));
                    patch.push(ep);
                }
            }
        }
        let balancer = if patch.is_empty() {
            None
        } else {
            match patch.finish() {
                Ok(b) => Some(b),
                Err(e) => {
                    for address in &joined {
                        src.metrics.forget_endpoint(&cfg.name, address);
                    }
                    return Err(e.to_string());
                }
            }
        };
        for address in &gone {
            src.metrics.forget_endpoint(&cfg.name, address);
        }
        let list = balancer
            .as_ref()
            .map(|b| b.endpoints().clone())
            .unwrap_or_default();
        if let Some(c) = &self.checker {
            c.patch(list.clone(), probes);
        }
        if let Some(d) = &self.detector {
            d.set_endpoints(list);
        }
        Ok(Cluster {
            index: Mutex::new(Some(index)),
            metrics: self.metrics.with_attempts(attempts),
            balancer,
            ..self.carried_over()
        })
    }

    /// The parts of this cluster that every discovery change keeps, with
    /// no endpoints yet.
    fn carried_over(&self) -> Cluster {
        Cluster {
            config: self.config.clone(),
            hash_header: self.hash_header.clone(),
            clock: Arc::clone(&self.clock),
            client: Arc::clone(&self.client),
            timeout: self.timeout,
            in_flight: Arc::clone(&self.in_flight),
            max_requests: self.max_requests,
            retry: self.retry.as_ref().map(|r| Retrying {
                policy: r.policy.clone(),
                budget: Arc::clone(&r.budget),
            }),
            detector: self.detector.clone(),
            checker: self.checker.clone(),
            jitter: SplitMix64::from_entropy(),
            metrics: self.metrics.with_attempts(Chunked::new()),
            index: Mutex::new(None),
            balancer: None,
        }
    }

    /// The same cluster balancing over the ready endpoints of `found`, as
    /// discovery changes it. Everything else carries on untouched: the
    /// pool, the in-flight count, the retry budget, the checker and the
    /// detector, which follow the new endpoints without restarting, so
    /// endpoints that stay keep their probe schedule. An endpoint that
    /// stays keeps its state and its attempt series; the series of one
    /// that goes are dropped. Refused when the balancer cannot take the
    /// set (Maglev's table holds at most 65,536 endpoints).
    fn rediscovered(
        &self,
        found: &Chunked<Arc<disc::Endpoint>>,
        src: &Sources<'_>,
    ) -> Result<Cluster, String> {
        let cfg = &self.config;
        let wanted = ready(found.iter().map(|e| &**e));
        let endpoints = carry(&wanted, Some(self.endpoints()), src.clock.now(), cfg);
        let balancer = if endpoints.is_empty() {
            None
        } else {
            let opts = options(cfg, src);
            Some(Balancer::new(endpoints, opts).map_err(|e| e.to_string())?)
        };
        let none = Chunked::new();
        let new = balancer.as_ref().map_or(&none, Balancer::endpoints);
        let old: HashMap<&str, usize> = self
            .endpoints()
            .iter()
            .enumerate()
            .map(|(i, e)| (&*e.address, i))
            .collect();
        let attempts: Chunked<_> = new
            .iter()
            .map(|e| match old.get(&*e.address) {
                Some(&i) => self
                    .metrics
                    .endpoint_attempts(i)
                    .cloned()
                    .unwrap_or_else(|| src.metrics.endpoint_attempts(&cfg.name, &e.address)),
                None => src.metrics.endpoint_attempts(&cfg.name, &e.address),
            })
            .collect();
        let kept: HashSet<&str> = new.iter().map(|e| &*e.address).collect();
        for e in self.endpoints() {
            if !kept.contains(&*e.address) {
                src.metrics.forget_endpoint(&cfg.name, &e.address);
            }
        }
        if let Some(c) = &self.checker {
            c.set_endpoints(new.clone());
        }
        if let Some(d) = &self.detector {
            d.set_endpoints(new.clone());
        }
        Ok(Cluster {
            metrics: self.metrics.with_attempts(attempts),
            balancer,
            ..self.carried_over()
        })
    }

    /// How much of a body of `length` to buffer for replay, if any.
    fn buffer_limit(&self, length: Option<u64>) -> Option<u64> {
        self.retry.as_ref()?.policy.buffer_limit(length)
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
            self.metrics.overflow();
            return None;
        }
        Some(Permit(self.in_flight.clone()))
    }

    /// The cluster as `GET /clusters` shows it, as of `now` on the
    /// cluster's clock and `wall` on the system clock.
    fn state(&self, now: std::time::Instant, wall: SystemTime) -> ClusterState {
        let b = self
            .balancer
            .as_ref()
            .map(Balancer::state)
            .unwrap_or_default();
        let widen = |v: &[u32]| v.iter().map(|&n| u64::from(n)).collect();
        ClusterState {
            name: self.config.name.clone(),
            policy: policy_name(self.config.policy).to_owned(),
            in_flight: i64::from(self.in_flight.load(Ordering::Acquire)),
            max_requests: u64::from(self.max_requests),
            retry_budget_balance: self.retry.as_ref().map(|r| r.budget.balance()),
            balancer: BalancerState {
                healthy_per_priority: widen(&b.healthy),
                total_per_priority: widen(&b.total),
                priority_load_percent: b.priority_load.iter().map(|&n| u64::from(n)).collect(),
                panic: b.panic,
            },
            endpoints: self
                .endpoints()
                .iter()
                .map(|ep| {
                    let s = ep.state.snapshot(now);
                    EndpointState {
                        address: ep.address.to_string(),
                        weight: u64::from(ep.weight),
                        priority: u64::from(ep.priority),
                        healthy: s.healthy,
                        last_probe_error: s.last_probe_error,
                        ejected: s.ejected,
                        ejected_until: s.ejected_until.map(|at| wall_time(at, now, wall)),
                        times_ejected: s.times_ejected,
                        last_eject_reason: s.last_eject_reason,
                        consecutive_gateway_failures: u64::from(s.consecutive_gateway_failures),
                        in_flight: s.in_flight,
                        ewma_latency_ms: s.latency_ms,
                        available: s.available,
                    }
                })
                .collect(),
        }
    }
}

/// The policy as the configuration names it.
fn policy_name(policy: edgefront_core::config::Policy) -> &'static str {
    use edgefront_core::config::Policy as P;
    match policy {
        P::RoundRobin => "round_robin",
        P::LeastLoaded => "least_loaded",
        P::Maglev => "maglev",
    }
}

/// The wall-clock time of `at` on a monotonic clock that reads `now` when
/// the system clock reads `wall`.
fn wall_time(at: std::time::Instant, now: std::time::Instant, wall: SystemTime) -> SystemTime {
    match at.checked_duration_since(now) {
        Some(ahead) => wall + ahead,
        None => wall - now.duration_since(at),
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

/// Records a routed request in `steer_requests_total` and
/// `steer_request_duration_seconds` when it is dropped: when the response
/// body is done, as steer recorded once its handler returned. A request
/// whose client goes away first is counted as steer counted it, with
/// nginx's 499.
struct Record {
    cluster: Arc<Cluster>,
    status: u16,
    started: Instant,
}

impl Record {
    /// The record of a request to `cluster`, a 499 until it is answered.
    fn new(cluster: &Arc<Cluster>, started: Instant) -> Record {
        Record {
            cluster: Arc::clone(cluster),
            status: 499,
            started,
        }
    }

    /// Records the request when `resp`'s body is done.
    fn attach(mut self, resp: Response<Body>) -> Response<Body> {
        self.status = resp.status().as_u16();
        resp.map(|inner| {
            BodyExt::boxed(Recorded {
                inner,
                _record: self,
            })
        })
    }
}

impl Drop for Record {
    fn drop(&mut self) {
        self.cluster
            .metrics
            .request(self.status, self.started.elapsed());
    }
}

/// A response body that records its request when dropped.
struct Recorded {
    inner: Body,
    _record: Record,
}

impl HttpBody for Recorded {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
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
        let version = Arc::new(AtomicU64::new(1));
        let metrics = Arc::new(SteerMetrics::new(Arc::new(Registry::new()), {
            let version = Arc::clone(&version);
            move || version.load(Ordering::Acquire)
        }));
        let runtime = Runtime::build(
            cfg,
            None,
            1,
            &Sources {
                clock: &clock,
                rng: &rng,
                metrics: &metrics,
                discovered: &HashMap::new(),
            },
        );
        Proxy {
            current: watch::Sender::new(Arc::new(runtime)),
            clock,
            rng,
            metrics,
            version,
            filters: RouteFilters::new(cfg.routes.len()),
            reloading: Mutex::new(()),
            draining: AtomicBool::new(false),
            conn_quota: edgefront_core::listener::ConnQuota::default(),
            gate: None,
            discovered: Mutex::new(HashMap::new()),
            discovered_versions: Mutex::new(HashMap::new()),
        }
    }

    /// Asks `gate` about each connection the listener accepts, before any
    /// handshake; a `false` closes it with nothing read. A limiter's
    /// listener-wide connection rate is such a gate.
    #[must_use]
    pub fn with_connection_gate(
        mut self,
        gate: Arc<dyn Fn(SocketAddr) -> bool + Send + Sync>,
    ) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Closes each client connection gracefully once it has carried the
    /// quota's requests or bytes (see
    /// [`edgefront_core::listener::ConnQuota`]); none by default.
    #[must_use]
    pub fn with_conn_quota(mut self, quota: edgefront_core::listener::ConnQuota) -> Self {
        self.conn_quota = quota;
        self
    }

    /// Runs requests on each route through that route's filter in
    /// `filters`, indexed as the configuration's routes are. Routes with
    /// no filter are proxied as before, at the cost of one branch. A proxy
    /// with filters refuses [`Proxy::reload`], since a new configuration
    /// can number its routes differently.
    #[must_use]
    pub fn with_filters(mut self, filters: RouteFilters) -> Self {
        self.filters = filters;
        self
    }

    fn runtime(&self) -> Arc<Runtime> {
        Arc::clone(&self.current.borrow())
    }

    /// steer's metric families, which the request path, the health
    /// checkers, the outlier detectors and reloads record into, and
    /// `/metrics` renders.
    pub fn metrics(&self) -> &Arc<SteerMetrics> {
        &self.metrics
    }

    /// The version of the configuration in force: 1 for the first, and
    /// one more for each reload applied.
    pub fn config_version(&self) -> u64 {
        self.current.borrow().version
    }

    /// Swaps in `cfg`, already validated, as steer's `Server.Reload` did,
    /// and returns its version. Endpoints keep their state by cluster name
    /// and address, and so do in-flight counts, connection pools and
    /// retry budgets whose settings are unchanged (see [`Cluster::new`]);
    /// requests already in flight finish on the configuration they started
    /// with, and [`Proxy::run_checks`] moves its checks to the new one.
    /// `listen` and `admin_listen` cannot change. A refusal is counted as
    /// a failed reload and leaves the current configuration serving.
    pub fn reload(&self, cfg: &Config) -> Result<u64, String> {
        let _held = self
            .reloading
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let old = self.runtime();
        let refusal = if cfg.listen != old.listen || cfg.admin_listen != old.admin_listen {
            Some("listen and admin_listen cannot change on reload; restart edgefront instead")
        } else if self.filters.filtered() > 0 {
            Some("routes with filters cannot be reloaded; restart edgefront instead")
        } else {
            None
        };
        if let Some(why) = refusal {
            self.metrics.reload_failed();
            return Err(why.to_owned());
        }
        let version = old.version + 1;
        let discovered = self.discovered();
        let runtime = Runtime::build(
            cfg,
            Some(&old),
            version,
            &Sources {
                clock: &self.clock,
                rng: &self.rng,
                metrics: &self.metrics,
                discovered: &discovered,
            },
        );
        let (clusters, routes) = (runtime.clusters.len(), runtime.routes);
        self.current.send_replace(Arc::new(runtime));
        self.version.store(version, Ordering::Release);
        self.metrics.reload_succeeded();
        tracing::info!(version, clusters, routes, "configuration loaded");
        Ok(version)
    }

    /// Starts draining: from now on every response the proxy gives,
    /// through its [`Handler`], closes its connection, as steer's
    /// `SetKeepAlivesEnabled(false)` did, so clients go elsewhere for
    /// their next request while the listener is still open.
    pub fn drain(&self) {
        self.draining.store(true, Ordering::Release);
    }

    /// Whether [`Proxy::drain`] was called.
    pub fn draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// Counts a reload that failed before reaching [`Proxy::reload`], such
    /// as an unreadable or invalid file (steer's `RecordFailedReload`).
    pub fn record_failed_reload(&self) {
        self.metrics.reload_failed();
    }

    /// The live state `GET /clusters` reports (steer's `Server.State`).
    pub fn state(&self) -> ProxyState {
        let rt = self.runtime();
        let now = self.clock.now();
        let wall = SystemTime::now();
        ProxyState {
            config_version: rt.version,
            loaded_at: rt.loaded_at,
            clusters: rt.clusters.iter().map(|c| c.state(now, wall)).collect(),
        }
    }

    /// The live endpoints of the cluster named `name` in the configuration
    /// in force, in configuration order, or `None` when there is no such
    /// cluster. Their state is what the balancer reads on every pick.
    pub fn endpoints(&self, name: &str) -> Option<Vec<Endpoint>> {
        let rt = self.runtime();
        rt.cluster(name).map(|c| c.endpoints().to_vec())
    }

    /// Runs every cluster's health checks, outlier detection ticks and
    /// discovery sources until `shutdown` resolves, then returns once each
    /// has stopped. The binary spawns it beside the listener; without it
    /// endpoints are never probed, ejections never decay (though
    /// consecutive failures still eject) and discovery never runs. After a
    /// reload the checks of replaced clusters stop and those of new ones
    /// start; a cluster discovery changes keeps its checks running, and
    /// they follow its endpoints. Discovery restarts for a cluster only
    /// when a reload changes its sources or listed endpoints.
    pub async fn run_checks(&self, shutdown: impl std::future::Future<Output = ()>) {
        let mut changes = self.current.subscribe();
        tokio::pin!(shutdown);
        let (tx, mut rx) = mpsc::channel::<Arrival>(1024);
        let mut running: HashMap<String, Discovering> = HashMap::new();
        let mut generation = 0u64;
        let mut synced = 0u64;
        let mut checkers: Vec<(Arc<Checker>, watch::Sender<bool>)> = Vec::new();
        let mut detectors: Vec<(Arc<Detector>, watch::Sender<bool>)> = Vec::new();
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            let rt = Arc::clone(&changes.borrow_and_update());
            if rt.version != synced {
                synced = rt.version;
                self.sync_discovery(&rt, &mut running, &tx, &mut generation);
            }
            let want_checkers: Vec<&Arc<Checker>> = rt
                .clusters
                .iter()
                .filter_map(|c| c.checker.as_ref())
                .collect();
            checkers.retain(|(c, stop)| {
                let keep = want_checkers.iter().any(|w| Arc::ptr_eq(w, c));
                if !keep {
                    stop.send_replace(true);
                }
                keep
            });
            for c in want_checkers {
                if !checkers.iter().any(|(have, _)| Arc::ptr_eq(have, c)) {
                    let (stop, stopped) = watch::channel(false);
                    let checker = Arc::clone(c);
                    tasks.spawn(async move { checker.run(until_stopped(stopped)).await });
                    checkers.push((Arc::clone(c), stop));
                }
            }
            let want_detectors: Vec<&Arc<Detector>> = rt
                .clusters
                .iter()
                .filter_map(|c| c.detector.as_ref())
                .collect();
            detectors.retain(|(d, stop)| {
                let keep = want_detectors.iter().any(|w| Arc::ptr_eq(w, d));
                if !keep {
                    stop.send_replace(true);
                }
                keep
            });
            for d in want_detectors {
                if !detectors.iter().any(|(have, _)| Arc::ptr_eq(have, d)) {
                    let (stop, stopped) = watch::channel(false);
                    let detector = Arc::clone(d);
                    tasks.spawn(async move { detector.run(until_stopped(stopped)).await });
                    detectors.push((Arc::clone(d), stop));
                }
            }
            while tasks.try_join_next().is_some() {}
            drop(rt);
            let done = tokio::select! {
                () = &mut shutdown => true,
                changed = changes.changed() => changed.is_err(),
                Some(first) = rx.recv() => {
                    self.arrive(first, &mut rx, &mut running);
                    false
                }
            };
            if done {
                break;
            }
        }
        running.clear();
        for (_, stop) in &checkers {
            stop.send_replace(true);
        }
        for (_, stop) in &detectors {
            stop.send_replace(true);
        }
        while tasks.join_next().await.is_some() {}
    }

    /// The retry budget's balance in the cluster named `name`, when it
    /// retries: how many retries it would allow now.
    pub fn retry_budget_balance(&self, name: &str) -> Option<f64> {
        let rt = self.runtime();
        let c = rt.cluster(name)?;
        c.retry.as_ref().map(|r| r.budget.balance())
    }

    /// A copy of the discovered sets, for building a runtime.
    fn discovered(&self) -> HashMap<String, Chunked<Arc<disc::Endpoint>>> {
        self.discovered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Starts discovery for each cluster of `rt` that has it and is not
    /// already running with the same sources and listed endpoints, and
    /// stops it for clusters that no longer have it. A cluster starting
    /// with listed endpoints balances over them at once; one whose sources
    /// a reload changed keeps its last discovered endpoints until a new
    /// source first reports.
    fn sync_discovery(
        &self,
        rt: &Runtime,
        running: &mut HashMap<String, Discovering>,
        tx: &mpsc::Sender<Arrival>,
        generation: &mut u64,
    ) {
        let wanted: HashMap<&str, &ClusterConfig> = rt
            .clusters
            .iter()
            .filter(|c| c.config.discovery.is_some())
            .map(|c| (c.config.name.as_str(), &c.config))
            .collect();
        running.retain(|name, d| wanted.get(name.as_str()).is_some_and(|c| d.started_for(c)));
        self.discovered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|name, _| wanted.contains_key(name.as_str()));
        self.discovered_versions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|name, _| wanted.contains_key(name.as_str()));
        for (name, cfg) in wanted {
            if running.contains_key(name) {
                continue;
            }
            *generation += 1;
            let d = Discovering::start(cfg, *generation, &self.metrics, tx);
            running.insert(name.to_owned(), d);
            let d = running.get_mut(name).expect("just inserted");
            let first = !self
                .discovered
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key(name);
            if first && d.merger.version() != d.published {
                self.publish(name, d, std::time::Instant::now());
            }
        }
    }

    /// Applies `first` and every update already waiting behind it, then
    /// rebuilds each cluster whose merged set moved, once.
    fn arrive(
        &self,
        first: Arrival,
        rx: &mut mpsc::Receiver<Arrival>,
        running: &mut HashMap<String, Discovering>,
    ) {
        let mut touched: HashMap<Arc<str>, std::time::Instant> = HashMap::new();
        let mut take = |a: Arrival| {
            let Some(d) = running.get_mut(&*a.cluster) else {
                return;
            };
            if d.generation != a.generation {
                return;
            }
            let source = a.update.source;
            let failed = matches!(a.update.kind, UpdateKind::Failed(_));
            if let UpdateKind::Failed(e) = &a.update.kind {
                tracing::warn!(
                    cluster = &*a.cluster,
                    source = d
                        .merger
                        .status()
                        .nth(source)
                        .map_or("", |s| s.name.as_str()),
                    error = e.as_str(),
                    "discovery source failed; keeping its last endpoints"
                );
            }
            let change = d.merger.apply(a.update, a.at);
            d.touch(change);
            let stale = d.merger.status().nth(source).is_some_and(|s| s.stale);
            d.metrics.source(source, failed, stale);
            touched.entry(a.cluster).or_insert(a.at);
        };
        take(first);
        // Coalesce what is already queued, but not without bound, so one
        // busy cluster cannot hold the others back for long.
        for _ in 0..4096 {
            match rx.try_recv() {
                Ok(a) => take(a),
                Err(_) => break,
            }
        }
        for (name, since) in touched {
            if let Some(d) = running.get_mut(&*name)
                && d.merger.version() != d.published
            {
                self.publish(&name, d, since);
            }
        }
    }

    /// Patches the cluster `name` with what `d`'s merged set changed since
    /// it last published, and swaps it in. Only the addresses touched are
    /// looked at, so at 100,000 endpoints a change of one costs about the
    /// same as at 10. A cluster that has to be set up from the whole set
    /// (the first set from a discovery started or restarted by a reload,
    /// or the set after one was refused) is built from the whole set
    /// instead, as [`Cluster::rediscovered`].
    fn publish(&self, name: &str, d: &mut Discovering, since: std::time::Instant) {
        d.published = d.merger.version();
        let touched = d.sync_list();
        d.metrics.endpoints(d.list.len());
        if d.list.len() > limits::MAX_DISCOVERED_ENDPOINTS {
            d.metrics.refused();
            d.resync = true;
            tracing::warn!(
                cluster = name,
                endpoints = d.list.len(),
                limit = limits::MAX_DISCOVERED_ENDPOINTS,
                "discovered too many endpoints; keeping the last set"
            );
            return;
        }
        let _held = self
            .reloading
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let rt = self.runtime();
        let Some(index) = rt.clusters.iter().position(|c| c.config.name == name) else {
            return;
        };
        let rebuilt = {
            let none = HashMap::new();
            let src = Sources {
                clock: &self.clock,
                rng: &self.rng,
                metrics: &self.metrics,
                discovered: &none,
            };
            let cluster = &rt.clusters[index];
            if d.resync {
                cluster.rediscovered(&d.list, &src)
            } else {
                let changes: Vec<(&str, Option<&disc::Endpoint>)> = touched
                    .iter()
                    .map(|a| (a.as_str(), d.merger.get(a)))
                    .collect();
                cluster.patched(&changes, &src)
            }
        };
        match rebuilt {
            Ok(cluster) => {
                d.resync = false;
                let ready = cluster.endpoints().len();
                self.discovered
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(name.to_owned(), d.list.clone());
                self.current
                    .send_replace(Arc::new(rt.with_cluster(index, Arc::new(cluster))));
                self.discovered_versions
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(name.to_owned(), d.published);
                d.metrics.applied(since.elapsed());
                tracing::debug!(
                    cluster = name,
                    version = d.published,
                    ready,
                    "discovered endpoints applied"
                );
            }
            Err(e) => {
                d.metrics.refused();
                d.resync = true;
                tracing::warn!(
                    cluster = name,
                    error = e.as_str(),
                    "cannot balance the discovered endpoints; keeping the last set"
                );
            }
        }
    }

    /// The version of the merged discovered set the cluster named `name`
    /// balances on: it rises by one for each source update that changed
    /// the set, so a caller that knows what it sent can tell when a change
    /// reached the balancer. `None` before discovery first applied a set.
    pub fn discovery_version(&self, name: &str) -> Option<u64> {
        self.discovered_versions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .copied()
    }

    /// The endpoints discovery last gave the cluster named `name`, ready
    /// or not and in address order, or `None` before it first gave any
    /// (or when the cluster does not discover). [`Proxy::endpoints`] has
    /// the ready ones the balancer uses.
    pub fn discovered_endpoints(&self, name: &str) -> Option<Vec<disc::Endpoint>> {
        self.discovered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .map(|found| {
                let mut all: Vec<disc::Endpoint> = found.iter().map(|e| (**e).clone()).collect();
                all.sort_unstable_by(|a, b| a.address.cmp(&b.address));
                all
            })
    }

    /// How many priority levels the cluster named `name` balances over.
    pub fn priority_levels(&self, name: &str) -> Option<usize> {
        let rt = self.runtime();
        rt.cluster(name)
            .map(|c| c.balancer.as_ref().map_or(0, Balancer::levels))
    }
    /// Answers one request from `peer`.
    pub async fn handle(&self, req: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
        let started = Instant::now();
        let rt = self.runtime();
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
                &Access::new(&parts, &id, peer, &rt.trusted, b"/"),
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
        let access = Access::new(&parts, &id, peer, &rt.trusted, target.clean_path());
        let Some(&(ri, ci)) = rt.router.route(&host, target.clean_path()) else {
            self.metrics.no_route();
            let resp = error(
                &id,
                StatusCode::NOT_FOUND,
                "no_route",
                "no route matches this host and path",
            );
            log(&access, started, &resp, "no_route");
            return resp;
        };
        let cluster = &rt.clusters[ci];
        let record = Record::new(cluster, started);
        let access = access.cluster(&cluster.config.name);
        let tls = parts.extensions.get::<Arc<TlsInfo>>().cloned();
        let client = tls.as_ref().and_then(|t| t.client.as_ref());
        if let Some(allowed) = &cluster.config.allowed_clients
            && !client.is_some_and(|c| admits(allowed, c))
        {
            let resp = error(
                &id,
                StatusCode::FORBIDDEN,
                "client_not_allowed",
                "this client certificate may not reach this route",
            );
            log(&access, started, &resp, "client_not_allowed");
            return record.attach(resp);
        }
        let fwd = Forwarding {
            trusted: Arc::clone(&rt.trusted),
            peer: access.peer,
            host: host.clone(),
            id: id.clone(),
            absolute: parts.uri.authority().is_some(),
            tls: tls.is_some(),
            client_spiffe_id: client
                .and_then(|c| c.spiffe_id.as_deref())
                .and_then(|s| HeaderValue::from_str(s).ok()),
        };
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
                    extensions: &parts.extensions,
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
                    fwd.apply(&mut headers);
                    let refresh = Refresh {
                        cluster: Arc::clone(cluster),
                        method: parts.method.clone(),
                        key: cluster.key(&parts.headers),
                        headers,
                        target: target.upstream_target().into_owned(),
                        fwd: fwd.clone(),
                        exchange: x,
                    };
                    tokio::spawn(refresh.run());
                    Some(resp)
                }
            };
            if let Some(mut resp) = resp {
                resp.headers_mut().insert(reqid::HEADER, id);
                log(&access, started, &resp, "");
                return record.attach(resp);
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
            return record.attach(resp);
        };
        let key = cluster.key(&parts.headers);
        let mut headers = parts.headers;
        let length = request_length(&headers, &incoming, parts.version);
        fwd.apply(&mut headers);

        // A gRPC call's own deadline bounds it too, and the endpoint is
        // told the time left.
        let call_deadline = grpc::timeout(&headers).map(|t| started + t);
        let deadline = match (cluster.timeout.map(|t| started + t), call_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if call_deadline.is_some()
            && let Some(at) = deadline
        {
            grpc::set_timeout(&mut headers, at.saturating_duration_since(Instant::now()));
        }
        let mut tried = Vec::new();
        let upstream_target = target.upstream_target();
        let exchange_fut = async {
            // steer reads a body it may replay before it picks.
            let Ok(body) = upstream_body(incoming, length, cluster.buffer_limit(length)).await
            else {
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
                    &mut tried,
                )
                .await
        };
        let result = match deadline {
            Some(at) => tokio::time::timeout_at(at, exchange_fut)
                .await
                .unwrap_or(Err(Failure::Timeout)),
            None => exchange_fut.await,
        };
        let access = access.tried(cluster.endpoints(), &tried);

        let failure = match result {
            Ok((resp, in_flight)) => {
                let Some(mut x) = exchange else {
                    // The common case records in the body it already has.
                    let mut resp = proxied(resp, deadline, permit, in_flight, Some(record));
                    resp.headers_mut().insert(reqid::HEADER, id);
                    log(&access, started, &resp, "");
                    return resp;
                };
                let resp = proxied(resp, deadline, permit, in_flight, None);
                let next = x.upstream_response(resp).await;
                let again = Again {
                    cluster,
                    method: &parts.method,
                    target: &upstream_target,
                    key,
                    fwd: &fwd,
                    deadline,
                };
                let mut resp = again.finish(x, next).await;
                resp.headers_mut().insert(reqid::HEADER, id);
                log(&access, started, &resp, "");
                return record.attach(resp);
            }
            Err(f) => f,
        };
        let (status, code, message, logged) = failure.describe();
        let resp = error(&id, status, code, message);
        let resp = deliver_generated(exchange, resp, code, &id);
        log(&access, started, &resp, logged);
        record.attach(resp)
    }
}

/// The verified client's SPIFFE ID, as the upstream sees it.
pub const X_CLIENT_SPIFFE_ID: HeaderName = HeaderName::from_static("x-client-spiffe-id");

/// Whether a cluster's `allowed_clients` admits `client`.
fn admits(allowed: &[String], client: &ClientIdentity) -> bool {
    allowed.iter().any(|p| match p.strip_prefix("dns:") {
        Some(name) => client
            .dns_names
            .iter()
            .any(|n| n.eq_ignore_ascii_case(name)),
        None => client
            .spiffe_id
            .as_deref()
            .is_some_and(|id| spiffe_matches(p, id)),
    })
}

/// How a request's headers are turned into the upstream's, kept so a
/// resend or a background refresh gets the same treatment.
#[derive(Clone)]
struct Forwarding {
    trusted: Arc<[IpPrefix]>,
    peer: Option<std::net::IpAddr>,
    host: String,
    id: HeaderValue,
    /// The client sent an absolute-form target.
    absolute: bool,
    /// The request came over TLS, so `X-Forwarded-Proto` is `https`.
    tls: bool,
    /// The verified client certificate's SPIFFE ID, sent upstream as
    /// `X-Client-Spiffe-Id`.
    client_spiffe_id: Option<HeaderValue>,
}

impl Forwarding {
    /// Hop-by-hop headers go, the forwarding headers are rewritten, and the
    /// request ID and, for an absolute-form target, the `Host` are set.
    fn apply(&self, headers: &mut HeaderMap) {
        strip_hop_by_hop(headers, true);
        forwarded::set_forwarded(
            headers,
            self.peer,
            self.tls,
            self.host.as_bytes(),
            &self.trusted,
        );
        headers.insert(reqid::HEADER, self.id.clone());
        // Only the proxy says who the client is: an inbound value is
        // dropped whatever it claims.
        headers.remove(&X_CLIENT_SPIFFE_ID);
        if let Some(id) = &self.client_spiffe_id {
            headers.insert(X_CLIENT_SPIFFE_ID, id.clone());
        }
        if self.absolute {
            // Go sends `Request.Host`, which an absolute-form target sets.
            headers.insert(
                header::HOST,
                HeaderValue::from_str(&self.host).expect("an authority"),
            );
        }
    }
}

/// What a filter's resend needs: the request's cluster, method, target,
/// hash key and forwarding, and the deadline its first try was held to.
struct Again<'a> {
    cluster: &'a Cluster,
    method: &'a Method,
    target: &'a [u8],
    key: Option<u64>,
    fwd: &'a Forwarding,
    deadline: Option<Instant>,
}

impl Again<'_> {
    /// Delivers what the upstream response phase decided, after sending
    /// the request once more, without a body, if it asked to. The resend
    /// takes a place under `max_requests` and keeps the first try's
    /// deadline.
    async fn finish(&self, mut x: Box<dyn Exchange>, next: Upstream) -> Response<Body> {
        let mut headers = match next {
            Upstream::Respond(resp) => return x.deliver(resp, Delivery::Upstream),
            Upstream::Resend(headers) => headers,
        };
        self.fwd.apply(&mut headers);
        let id = &self.fwd.id;
        let Some(permit) = self.cluster.acquire() else {
            let resp = error(
                id,
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded",
                "too many concurrent requests to this service",
            );
            return x.deliver(resp, Delivery::Generated("overloaded"));
        };
        let mut tried = Vec::new();
        let send = self.cluster.forward(
            self.method,
            headers,
            self.target,
            UpstreamBody::Empty,
            Some(0),
            self.key,
            &mut tried,
        );
        let result = match self.deadline {
            Some(at) => tokio::time::timeout_at(at, send)
                .await
                .unwrap_or(Err(Failure::Timeout)),
            None => send.await,
        };
        match result {
            Ok((resp, in_flight)) => {
                let resp = proxied(resp, self.deadline, permit, in_flight, None);
                match x.upstream_response(resp).await {
                    Upstream::Respond(resp) => x.deliver(resp, Delivery::Upstream),
                    Upstream::Resend(_) => {
                        let resp = error(
                            id,
                            StatusCode::BAD_GATEWAY,
                            "bad_gateway",
                            "the upstream failed to answer",
                        );
                        x.deliver(resp, Delivery::Generated("resend_limit"))
                    }
                }
            }
            Err(f) => {
                let (status, code, message, _) = f.describe();
                x.deliver(error(id, status, code, message), Delivery::Generated(code))
            }
        }
    }
}

impl Cluster {
    /// Sends the request to the endpoints the balancer picks, as steer's
    /// round tripper did: one attempt, then retries while the policy, the
    /// balancer and the budget allow. Each endpoint tried is pushed onto
    /// `tried` as it is picked. Every attempt is reported to the outlier
    /// detector and observed as latency (at least [`FAILURE_PENALTY`] when
    /// it failed or answered 5xx). An attempt cancelled by the cluster
    /// timeout observes nothing, as steer skipped requests whose context
    /// ended.
    #[allow(clippy::too_many_arguments)]
    async fn forward(
        &self,
        method: &Method,
        mut headers: HeaderMap,
        target: &[u8],
        body: UpstreamBody,
        length: Option<u64>,
        key: Option<u64>,
        tried: &mut Vec<usize>,
    ) -> Result<(Response<ResponseBody>, InFlight), Failure> {
        let uri_for = |address: &str| {
            Uri::try_from(format!(
                "http://{address}{}",
                String::from_utf8_lossy(target)
            ))
            .map_err(|_| Failure::Proxy)
        };
        if let Some(r) = &self.retry {
            r.budget.deposit();
        }
        let safe = retry::idempotent(method, &headers);
        let replayable = body.replayable();
        set_framing(&mut headers, method, length);
        let per_try = self.retry.as_ref().and_then(|r| r.policy.per_try_timeout());
        let mut stream = Some(body);

        let Some(mut i) = self.balancer.as_ref().and_then(|b| b.pick(key, tried)) else {
            return Err(Failure::NoHealthyUpstream);
        };
        for attempt in 0u32.. {
            tried.push(i);
            let endpoint = &self.endpoints()[i];
            let in_flight = endpoint.state.track();
            let body = match stream.as_ref() {
                Some(UpstreamBody::Empty) => body::empty(),
                Some(UpstreamBody::Buffered(b)) => body::full(b.clone()),
                // Only a body that cannot be replayed streams, and it
                // is never sent twice.
                Some(UpstreamBody::Stream(_)) | None => match stream.take() {
                    Some(UpstreamBody::Stream(b)) => b,
                    _ => body::empty(),
                },
            };
            let mut out = Request::new(body);
            *out.method_mut() = method.clone();
            *out.uri_mut() = uri_for(&endpoint.address)?;
            *out.version_mut() = Version::HTTP_11;
            *out.headers_mut() = headers.clone();
            let began = self.clock.now();
            let result = match retry::per_try(&*self.clock, per_try, self.client.send(out)).await {
                Ok(r) => r.map(|(resp, _)| resp).map_err(Outcome::of_error),
                Err(retry::PerTryTimeout) => Err(Outcome::Timeout),
            };
            let now = self.clock.now();
            let outcome = match &result {
                Ok(resp) => Outcome::of_status(resp.status().as_u16()),
                Err(o) => *o,
            };
            self.metrics.attempt(i, outcome.into());
            if let Some(d) = &self.detector {
                d.report(endpoint, outcome.is_gateway_failure(), outcome.is_failure());
            }
            let elapsed = now.saturating_duration_since(began);
            let sample = if outcome == Outcome::Success {
                elapsed
            } else {
                elapsed.max(FAILURE_PENALTY)
            };
            endpoint.state.observe_latency(sample, now);

            let next = self.retry.as_ref().and_then(|r| {
                if !r.policy.retryable(attempt, replayable, safe, outcome) {
                    return None;
                }
                let next = self.balancer.as_ref()?.pick(key, tried)?;
                if !r.budget.try_withdraw() {
                    self.metrics.budget_exhausted();
                    tracing::debug!(cluster = self.config.name, "retry budget exhausted");
                    return None;
                }
                Some((
                    next,
                    r.policy
                        .backoff(attempt, health::unit(self.jitter.next_u64())),
                ))
            });
            let Some((next, wait)) = next else {
                return match result {
                    Ok(resp) => Ok((resp, in_flight)),
                    Err(o) => Err(Failure::Attempt(o)),
                };
            };
            if let Ok(resp) = result {
                drain(resp).await;
            }
            drop(in_flight);
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            self.metrics.retry();
            i = next;
        }
        unreachable!("the attempt counter ends the loop long before it wraps")
    }
}

/// Reads up to 4 KiB of a response that will not be delivered, so a short
/// body leaves its connection ready for reuse, as steer did before a
/// retry, then drops it.
async fn drain(resp: Response<ResponseBody>) {
    const DRAIN: usize = 4 << 10;
    let mut body = resp.into_body();
    let mut read = 0;
    while read < DRAIN {
        match body.frame().await {
            Some(Ok(frame)) => read += frame.data_ref().map_or(0, Bytes::len),
            Some(Err(_)) | None => return,
        }
    }
}

/// An upstream response on its way to the client, with the hop-by-hop
/// headers removed and its body cut at the deadline.
fn proxied(
    resp: Response<ResponseBody>,
    deadline: Option<Instant>,
    permit: Permit,
    in_flight: InFlight,
    record: Option<Record>,
) -> Response<Body> {
    let (mut head, incoming) = resp.into_parts();
    strip_hop_by_hop(&mut head.headers, false);
    // Go writes the standard reason phrase whatever the upstream sent.
    head.extensions.remove::<hyper::ext::ReasonPhrase>();
    let record = record.map(|mut r| {
        r.status = head.status.as_u16();
        r
    });
    let body = Proxied {
        inner: incoming,
        deadline: deadline.map(|at| Box::pin(tokio::time::sleep_until(at))),
        _permit: permit,
        _in_flight: in_flight,
        _record: record,
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
    fwd: Forwarding,
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
            fwd,
            mut exchange,
        } = self;
        let id = &fwd.id;
        let Some(permit) = cluster.acquire() else {
            let resp = error(
                id,
                StatusCode::SERVICE_UNAVAILABLE,
                "overloaded",
                "too many concurrent requests to this service",
            );
            drop(exchange.deliver(resp, Delivery::Generated("overloaded")));
            return;
        };
        let deadline = cluster.timeout.map(|t| Instant::now() + t);
        let mut tried = Vec::new();
        let send = cluster.forward(
            &method,
            headers,
            &target,
            UpstreamBody::Empty,
            Some(0),
            key,
            &mut tried,
        );
        let result = match deadline {
            Some(at) => tokio::time::timeout_at(at, send)
                .await
                .unwrap_or(Err(Failure::Timeout)),
            None => send.await,
        };
        match result {
            Ok((resp, in_flight)) => {
                let resp = proxied(resp, deadline, permit, in_flight, None);
                let next = exchange.upstream_response(resp).await;
                let again = Again {
                    cluster: &cluster,
                    method: &method,
                    target: &target,
                    key,
                    fwd: &fwd,
                    deadline,
                };
                drop(again.finish(exchange, next).await);
            }
            Err(f) => {
                let (status, code, message, _) = f.describe();
                let resp = error(id, status, code, message);
                drop(exchange.deliver(resp, Delivery::Generated(code)));
            }
        }
    }
}

/// One update from a cluster's discovery source, on its way to the
/// cluster's merger.
struct Arrival {
    cluster: Arc<str>,
    /// Which start of the cluster's discovery it comes from; updates from
    /// one a reload replaced are dropped.
    generation: u64,
    update: Update,
    at: std::time::Instant,
}

/// One cluster's discovery while it runs.
struct Discovering {
    /// The sources and listed endpoints it was started for.
    discovery: edgefront_core::config::Discovery,
    listed: Vec<edgefront_core::config::Endpoint>,
    generation: u64,
    merger: Merger,
    /// The merger version last published to the cluster.
    published: u64,
    /// The merged set as last published, in no particular order, with
    /// each address's index in it; patched by the addresses touched.
    list: Chunked<Arc<disc::Endpoint>>,
    at: HashMap<String, usize>,
    /// The addresses the merged set changed at since the last publish.
    touched: HashSet<String>,
    /// Whether the cluster is to be built from the whole set next time,
    /// not patched: true at the start, since the cluster may hold
    /// endpoints from before (the listed ones, or a discovery a reload
    /// replaced), and after a set was refused.
    resync: bool,
    metrics: DiscoveryMetrics,
    tasks: Vec<tokio::task::AbortHandle>,
}

impl Discovering {
    /// Starts `cfg`'s sources, each sending into `tx`, with the listed
    /// endpoints as one more source after them.
    fn start(
        cfg: &ClusterConfig,
        generation: u64,
        metrics: &SteerMetrics,
        tx: &mpsc::Sender<Arrival>,
    ) -> Discovering {
        let discovery = cfg.discovery.clone().expect("a discovering cluster");
        let cluster: Arc<str> = cfg.name.as_str().into();
        let sources: Vec<Box<dyn disc::Source>> = discovery
            .sources
            .iter()
            .map(|s| disc::from_config(s, &discovery.dns_servers))
            .collect();
        let mut names: Vec<String> = sources.iter().map(|s| s.name()).collect();
        if !cfg.endpoints.is_empty() {
            names.push("static".to_owned());
        }
        let mut merger = Merger::new(names.clone());
        let mut tasks = Vec::new();
        for (index, source) in sources.into_iter().enumerate() {
            let (stx, mut srx) = mpsc::channel(64);
            tasks.push(tokio::spawn(source.run(stx)).abort_handle());
            let tx = tx.clone();
            let cluster = Arc::clone(&cluster);
            tasks.push(
                tokio::spawn(async move {
                    while let Some(kind) = srx.recv().await {
                        let arrival = Arrival {
                            cluster: Arc::clone(&cluster),
                            generation,
                            update: Update {
                                source: index,
                                kind,
                            },
                            at: std::time::Instant::now(),
                        };
                        if tx.send(arrival).await.is_err() {
                            return;
                        }
                    }
                })
                .abort_handle(),
            );
        }
        if !cfg.endpoints.is_empty() {
            let listed = cfg
                .endpoints
                .iter()
                .map(|e| disc::Endpoint {
                    address: e.address.clone(),
                    weight: e.weight,
                    priority: e.priority,
                    zone: None,
                    ready: true,
                })
                .collect();
            let update = Update {
                source: names.len() - 1,
                kind: UpdateKind::Set(listed),
            };
            merger.apply(update, std::time::Instant::now());
        }
        let metrics = metrics.discovery(&cfg.name, &names);
        let touched = merger.endpoints().map(|e| e.address.clone()).collect();
        Discovering {
            discovery,
            listed: cfg.endpoints.clone(),
            generation,
            merger,
            published: 0,
            list: Chunked::new(),
            at: HashMap::new(),
            touched,
            resync: true,
            metrics,
            tasks,
        }
    }

    /// Notes the addresses `change` touched, for the next publish.
    fn touch(&mut self, change: disc::Change) {
        let addresses = change
            .added
            .into_iter()
            .chain(change.changed)
            .map(|e| e.address);
        self.touched.extend(addresses.chain(change.removed));
    }

    /// Brings `list` up to the merged set at the addresses touched, and
    /// returns them.
    fn sync_list(&mut self) -> Vec<String> {
        let touched: Vec<String> = self.touched.drain().collect();
        for address in &touched {
            match (self.at.get(address).copied(), self.merger.get(address)) {
                (None, None) => {}
                (Some(i), None) => {
                    self.list.swap_remove(i);
                    self.at.remove(address);
                    if let Some(moved) = self.list.get(i) {
                        self.at.insert(moved.address.clone(), i);
                    }
                }
                (Some(i), Some(e)) => {
                    if *self.list[i] != *e {
                        self.list.set(i, Arc::new(e.clone()));
                    }
                }
                (None, Some(e)) => {
                    self.at.insert(address.clone(), self.list.len());
                    self.list.push(Arc::new(e.clone()));
                }
            }
        }
        touched
    }

    /// Whether it runs what `cfg` asks for.
    fn started_for(&self, cfg: &ClusterConfig) -> bool {
        cfg.discovery.as_ref() == Some(&self.discovery) && cfg.endpoints == self.listed
    }
}

impl Drop for Discovering {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        self.metrics.retire();
    }
}

/// Resolves once `stopped` turns true or its sender is gone.
async fn until_stopped(mut stopped: tokio::sync::watch::Receiver<bool>) {
    let _ = stopped.wait_for(|s| *s).await;
}

impl reload::Service for Proxy {
    fn reload(&self, config: &Config) -> Result<u64, String> {
        Proxy::reload(self, config)
    }

    fn record_failed_reload(&self) {
        Proxy::record_failed_reload(self);
    }

    fn drain(&self) {
        Proxy::drain(self);
    }

    fn draining(&self) -> bool {
        Proxy::draining(self)
    }

    fn state(&self) -> ProxyState {
        Proxy::state(self)
    }

    fn write_metrics(&self, out: &mut String) {
        self.metrics().registry().write(out);
    }

    fn tls_metrics(&self, in_progress: Box<dyn Fn() -> u64 + Send + Sync>) -> TlsMetrics {
        self.metrics().tls(in_progress)
    }

    async fn background(self: Arc<Self>, stop: Pin<Box<dyn Future<Output = ()> + Send>>) {
        self.run_checks(stop).await;
    }
}

impl Handler for Proxy {
    type Body = Body;

    fn conn_quota(&self) -> edgefront_core::listener::ConnQuota {
        self.conn_quota
    }

    fn admit_connection(&self, peer: SocketAddr) -> bool {
        self.gate.as_ref().is_none_or(|g| g(peer))
    }

    async fn call(self: Arc<Self>, req: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
        let h2 = req.version() == Version::HTTP_2;
        let grpc = grpc::is_grpc(req.headers());
        let mut resp = self.handle(req, peer).await;
        if grpc && let Some(Answered(message)) = resp.extensions_mut().remove::<Answered>() {
            resp = grpc::trailers_only(resp, message);
        }
        // HTTP/2 has no `Connection` header; the listener's GOAWAY ends
        // such a connection when the drain stops it.
        if self.draining() && !h2 {
            resp.headers_mut()
                .insert(header::CONNECTION, HeaderValue::from_static("close"));
        }
        resp
    }
}

/// Why the proxy answered instead of an upstream.
enum Failure {
    NoHealthyUpstream,
    Timeout,
    ClientBody,
    /// The last attempt got no response.
    Attempt(Outcome),
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
            // steer's error handler: an attempt that timed out, by the
            // per-try timeout or the upstream's, is a 504.
            Failure::Attempt(Outcome::Timeout) => (
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "the upstream did not answer in time",
                "upstream_timeout",
            ),
            Failure::Attempt(o) => (
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                "the upstream failed to answer",
                o.as_str(),
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
        // HTTP/2 names the host in `:authority` (RFC 9113 section 8.3.1).
        if version == Version::HTTP_2
            && let Some(a) = uri.authority()
        {
            return Ok(a.as_str());
        }
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

/// Marks a response the proxy wrote itself, with its message, so a gRPC
/// call gets it as a gRPC status instead.
#[derive(Clone, Copy)]
struct Answered(&'static str);

/// One of steer's JSON errors.
fn error(
    id: &HeaderValue,
    status: StatusCode,
    code: &str,
    message: &'static str,
) -> Response<Body> {
    // The ID passed `reqid::valid` or is hex, and the code and message are
    // fixed, so nothing needs JSON escaping. Go's encoder ends with "\n".
    let id_text = id.to_str().expect("request IDs are ASCII");
    let text = format!(
        "{{\"error\":{{\"code\":\"{code}\",\"message\":\"{message}\",\"request_id\":\"{id_text}\"}}}}\n"
    );
    let mut resp = Response::new(body::full(text));
    *resp.status_mut() = status;
    resp.extensions_mut().insert(Answered(message));
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
fn request_length(headers: &HeaderMap, body: &Incoming, version: Version) -> Option<u64> {
    if headers.contains_key(header::TRANSFER_ENCODING) {
        return None;
    }
    // HTTP/2 frames bodies itself: without `content-length` a body that
    // did not end with its headers streams, as chunked would.
    if version == Version::HTTP_2 && !body.is_end_stream() {
        return body.size_hint().exact();
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

/// The body to send upstream, as read before the first attempt.
enum UpstreamBody {
    /// No body: it can be sent again.
    Empty,
    /// A body read whole within the replay limit: it can be sent again.
    Buffered(Bytes),
    /// A body that streams to the first endpoint only.
    Stream(Body),
}

impl UpstreamBody {
    /// Whether every attempt can send the whole body.
    fn replayable(&self) -> bool {
        !matches!(self, UpstreamBody::Stream(_))
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
) -> Result<UpstreamBody, ()> {
    if length == Some(0) {
        return Ok(UpstreamBody::Empty);
    }
    let Some(limit) = replay_limit else {
        return Ok(UpstreamBody::Stream(
            incoming.map_err(BoxError::from).boxed(),
        ));
    };
    let mut buf = BytesMut::new();
    while (buf.len() as u64) <= limit {
        match incoming.frame().await {
            None => return Ok(UpstreamBody::Buffered(buf.freeze())),
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
    Ok(UpstreamBody::Stream(BodyExt::boxed(body)))
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
    inner: ResponseBody,
    deadline: Option<Pin<Box<Sleep>>>,
    _permit: Permit,
    _in_flight: InFlight,
    /// Records the request once the body is done, when nothing replaces
    /// the body on its way to the client.
    _record: Option<Record>,
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
    /// The endpoints tried, in order, as steer's `endpoints`.
    endpoints: Vec<&'a str>,
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
            endpoints: Vec::new(),
        }
    }

    fn cluster(self, cluster: &'a str) -> Self {
        Access { cluster, ..self }
    }

    fn tried(self, eps: &'a Chunked<Endpoint>, tried: &[usize]) -> Self {
        let endpoints = tried.iter().map(|&i| &*eps[i].address).collect();
        Access { endpoints, ..self }
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
                endpoint = a.endpoints.last().copied().unwrap_or_default(),
                endpoints = ?a.endpoints,
                attempts = a.endpoints.len(),
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
