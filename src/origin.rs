//! The origin pool: an ordered list of origins, active and passive health
//! checks, timeouts, and failover.
//!
//! Requests go to the first healthy origin in configured order. Requests
//! without a body move on to the next origin when one cannot be reached or
//! times out. If every origin is marked down, they are tried anyway in
//! order, since health data can lag behind a recovery.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use http::header::{self, HeaderMap, HeaderValue};
use http::uri::{Authority, PathAndQuery, Scheme};
use http::{Method, Request, Response, Uri};
use hyper::body::Incoming;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tokio::task::JoinHandle;

use crate::respond::{Body, empty};

pub type Client = HyperClient<HttpConnector, Body>;

/// Builds the shared HTTP/1.1 client for origins and peers.
pub fn client(connect_timeout: Duration) -> Client {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(connect_timeout));
    connector.set_nodelay(true);
    connector.enforce_http(true);
    HyperClient::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(256)
        .build(connector)
}

#[derive(Debug, Clone)]
pub struct HealthSettings {
    pub path: PathAndQuery,
    pub interval: Duration,
    pub timeout: Duration,
    /// Consecutive successes before a down origin is used again.
    pub rise: u32,
    /// Consecutive failures before an origin is marked down.
    pub fall: u32,
}

impl Default for HealthSettings {
    fn default() -> Self {
        HealthSettings {
            path: PathAndQuery::from_static("/healthz"),
            interval: Duration::from_secs(2),
            timeout: Duration::from_secs(1),
            rise: 2,
            fall: 3,
        }
    }
}

/// Why a request to the origins failed. Copyable so one failure can be
/// shared by every collapsed waiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginError {
    /// No connection could be made.
    Connect,
    /// No response headers in time.
    Timeout,
    /// The connection broke or the response was malformed.
    Protocol,
}

#[derive(Debug)]
pub struct Origin {
    pub name: String,
    pub authority: Authority,
    healthy: AtomicBool,
    successes: AtomicU32,
    failures: AtomicU32,
    pub requests: AtomicU64,
    pub errors: AtomicU64,
}

impl Origin {
    pub fn new(name: &str, authority: Authority) -> Self {
        Origin {
            name: name.to_string(),
            authority,
            healthy: AtomicBool::new(true),
            successes: AtomicU32::new(0),
            failures: AtomicU32::new(0),
            requests: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    fn record(&self, ok: bool, rise: u32, fall: u32) {
        if ok {
            self.failures.store(0, Ordering::Relaxed);
            let n = self.successes.fetch_add(1, Ordering::Relaxed) + 1;
            if n >= rise && !self.healthy.swap(true, Ordering::Relaxed) {
                tracing::warn!(origin = %self.name, "origin is healthy again");
            }
        } else {
            self.successes.store(0, Ordering::Relaxed);
            let n = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
            if n >= fall && self.healthy.swap(false, Ordering::Relaxed) {
                tracing::warn!(origin = %self.name, "origin marked down");
            }
        }
    }
}

/// A request to send to whichever origin is chosen.
pub struct Outbound {
    pub method: Method,
    pub path: PathAndQuery,
    pub headers: HeaderMap,
    /// `None` for requests without a body, which may be retried elsewhere.
    pub body: Option<Body>,
}

pub struct OriginPool {
    origins: Vec<Arc<Origin>>,
    client: Client,
    response_timeout: Duration,
    health: HealthSettings,
}

impl OriginPool {
    pub fn new(
        origins: Vec<Arc<Origin>>,
        client: Client,
        response_timeout: Duration,
        health: HealthSettings,
    ) -> Self {
        OriginPool {
            origins,
            client,
            response_timeout,
            health,
        }
    }

    pub fn origins(&self) -> &[Arc<Origin>] {
        &self.origins
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Healthy origins first, in configured order, then the rest.
    fn candidates(&self) -> Vec<Arc<Origin>> {
        let (mut up, down): (Vec<_>, Vec<_>) =
            self.origins.iter().cloned().partition(|o| o.is_healthy());
        up.extend(down);
        up
    }

    fn uri(origin: &Origin, path: &PathAndQuery) -> Uri {
        Uri::builder()
            .scheme(Scheme::HTTP)
            .authority(origin.authority.clone())
            .path_and_query(path.clone())
            .build()
            .expect("scheme, authority and path are each valid")
    }

    /// Sends the request, failing over between origins when it has no body.
    pub async fn send(
        &self,
        mut out: Outbound,
    ) -> Result<(Response<Incoming>, Arc<Origin>), OriginError> {
        let retryable = out.body.is_none();
        let mut last = OriginError::Connect;
        for origin in self.candidates() {
            let mut req = Request::new(out.body.take().unwrap_or_else(empty));
            *req.method_mut() = out.method.clone();
            *req.uri_mut() = Self::uri(&origin, &out.path);
            *req.headers_mut() = out.headers.clone();
            origin.requests.fetch_add(1, Ordering::Relaxed);
            let result =
                tokio::time::timeout(self.response_timeout, self.client.request(req)).await;
            let err = match result {
                Ok(Ok(resp)) => {
                    origin.record(true, self.health.rise, self.health.fall);
                    return Ok((resp, origin));
                }
                Ok(Err(e)) if e.is_connect() => OriginError::Connect,
                Ok(Err(_)) => OriginError::Protocol,
                Err(_) => OriginError::Timeout,
            };
            origin.errors.fetch_add(1, Ordering::Relaxed);
            if err != OriginError::Protocol {
                origin.record(false, self.health.rise, self.health.fall);
            }
            tracing::debug!(origin = %origin.name, error = ?err, "origin request failed");
            last = err;
            if !retryable {
                break;
            }
        }
        Err(last)
    }

    /// Probes one origin's health path.
    async fn probe(&self, origin: &Origin) -> bool {
        let mut req = Request::new(empty());
        *req.uri_mut() = Self::uri(origin, &self.health.path);
        let host = HeaderValue::from_str(origin.authority.as_str())
            .expect("authority is a valid header value");
        req.headers_mut().insert(header::HOST, host);
        match tokio::time::timeout(self.health.timeout, self.client.request(req)).await {
            Ok(Ok(resp)) => resp.status().is_success(),
            _ => false,
        }
    }

    /// Runs one round of active health checks, probing origins in parallel.
    pub async fn check_all(self: &Arc<Self>) {
        let mut probes = tokio::task::JoinSet::new();
        for origin in &self.origins {
            let (pool, origin) = (self.clone(), origin.clone());
            probes.spawn(async move {
                let ok = pool.probe(&origin).await;
                origin.record(ok, pool.health.rise, pool.health.fall);
            });
        }
        while probes.join_next().await.is_some() {}
    }

    /// Starts the periodic health checker.
    pub fn spawn_health_checks(self: &Arc<Self>) -> JoinHandle<()> {
        let pool = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(pool.health.interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                pool.check_all().await;
            }
        })
    }
}
