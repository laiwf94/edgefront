//! Active HTTP health checks, ported from steer's `internal/health`.
//!
//! Each endpoint is probed on its own schedule: at once when the checker
//! starts, then every interval plus a random jitter, so that probes from
//! many proxies do not arrive in lockstep. A probe is a `GET` of the
//! configured path and passes on any 2xx response within the timeout;
//! redirects are not followed and count as failures. An endpoint becomes
//! unhealthy after `unhealthy_threshold` failures in a row and healthy
//! again after `healthy_threshold` passes in a row, so one lost probe does
//! not flap it. The verdict lives in the endpoint's [`State`], where the
//! balancer reads it.
//!
//! As in steer, every probe opens a new connection (`Connection: close`,
//! no pool), so a pass proves a new connection can be made; the timeout
//! covers the connect, the response head and the first 4 KiB of body,
//! which is all that is read; and a response head over 16 KiB fails. The
//! wait between probes runs on the injected [`Clock`], the probe's own
//! timeout on the runtime's. A probe cut short by shutdown says nothing
//! about the endpoint and is not recorded.
//!
//! [`State`]: crate::endpoint::State

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, Bytes};
use edgefront_core::clock::Clock;
use edgefront_core::config::HealthCheck;
use edgefront_core::http1::{Client, ClientError, ClientLimits};
use http::header::{CONNECTION, HOST, HeaderValue, USER_AGENT};
use http::{Method, Request, Uri};
use http_body_util::{BodyExt, Empty};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::balancer::rng::SplitMix64;
use crate::endpoint::Endpoint;

/// The `User-Agent` of every probe. It keeps steer's value, so upstreams
/// that recognise health checks by it go on doing so.
pub const USER_AGENT_VALUE: &str = "steer-health-check";
/// The largest response head a probe accepts.
pub const MAX_RESPONSE_HEADER_BYTES: usize = 16 << 10;
/// How much of a probe's response body is read before the connection is
/// dropped.
pub const MAX_BODY_READ: usize = 4 << 10;

/// Told when an endpoint's verdict flips: the endpoint, its new verdict
/// and, when it went down, the failure that tipped it.
pub type ChangeFn = Box<dyn Fn(&Endpoint, bool, &str) + Send + Sync>;

/// Probes a set of endpoints.
pub struct Checker {
    cfg: HealthCheck,
    eps: Vec<Endpoint>,
    clock: Arc<dyn Clock>,
    client: Client<Empty<Bytes>>,
    random: Box<dyn Fn() -> f64 + Send + Sync>,
    on_change: Option<ChangeFn>,
}

impl fmt::Debug for Checker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Checker")
            .field("cfg", &self.cfg)
            .field("endpoints", &self.eps.len())
            .finish_non_exhaustive()
    }
}

impl Checker {
    /// A checker for `eps` with jitter drawn at random. Must be called
    /// inside a tokio runtime, which the probe client needs.
    pub fn new(
        cfg: HealthCheck,
        eps: Vec<Endpoint>,
        clock: Arc<dyn Clock>,
        on_change: Option<ChangeFn>,
    ) -> Checker {
        let rng = SplitMix64::from_entropy();
        let client = Client::new(&ClientLimits {
            connect_timeout: cfg.timeout,
            response_header_timeout: Some(cfg.timeout),
            pool_idle_timeout: Duration::from_secs(1),
            max_idle_per_host: 0,
            max_response_header_bytes: MAX_RESPONSE_HEADER_BYTES,
        });
        Checker {
            cfg,
            eps,
            clock,
            client,
            random: Box::new(move || unit(rng.next_u64())),
            on_change,
        }
    }

    /// Replaces the jitter source with `random`, which must return values
    /// in [0, 1), for tests that need a known schedule.
    pub fn with_random(mut self, random: impl Fn() -> f64 + Send + Sync + 'static) -> Checker {
        self.random = Box::new(random);
        self
    }

    /// The endpoints being probed.
    pub fn endpoints(&self) -> &[Endpoint] {
        &self.eps
    }

    /// The wait before an endpoint's next probe: the interval plus a fresh
    /// random fraction of the jitter.
    pub fn next_wait(&self) -> Duration {
        let jitter = (self.random)() * self.cfg.jitter.as_nanos() as f64;
        // Like Go's conversion of a float to a Duration, this truncates.
        self.cfg.interval + Duration::from_nanos(jitter as u64)
    }

    /// Probes every endpoint until `shutdown` resolves, then returns once
    /// every probe loop has stopped. A probe in flight at shutdown is
    /// abandoned without being recorded.
    pub async fn run(self: Arc<Self>, shutdown: impl Future<Output = ()>) {
        let (stop, stopped) = watch::channel(false);
        let mut loops = JoinSet::new();
        for i in 0..self.eps.len() {
            let checker = Arc::clone(&self);
            let stopped = stopped.clone();
            loops.spawn(async move { checker.probe_loop(i, stopped).await });
        }
        shutdown.await;
        stop.send_replace(true);
        while loops.join_next().await.is_some() {}
    }

    async fn probe_loop(&self, i: usize, mut stopped: watch::Receiver<bool>) {
        let ep = &self.eps[i];
        loop {
            tokio::select! {
                biased;
                () = until_stopped(&mut stopped) => return,
                _ = self.check_once(ep) => {}
            }
            let timer = self.clock.after(self.next_wait());
            tokio::select! {
                biased;
                () = until_stopped(&mut stopped) => return,
                _ = timer => {}
            }
        }
    }

    /// Probes `ep` once and applies the result to its state. Returns the
    /// verdict and whether it changed; a change is also reported to the
    /// change callback.
    pub async fn check_once(&self, ep: &Endpoint) -> (bool, bool) {
        let result = self.probe(ep).await;
        let reason = result.as_ref().err().map_or("", String::as_str);
        let (healthy, changed) = ep.state.record_probe(
            result.is_ok(),
            reason,
            self.cfg.healthy_threshold,
            self.cfg.unhealthy_threshold,
        );
        if changed && let Some(f) = &self.on_change {
            f(ep, healthy, reason);
        }
        (healthy, changed)
    }

    /// Probes `ep` once without recording anything. The error says why the
    /// probe failed, in the words the admin API shows as the endpoint's
    /// last probe error.
    pub async fn probe(&self, ep: &Endpoint) -> Result<(), String> {
        let target = format!("http://{}{}", ep.address, self.cfg.path);
        let uri: Uri = target
            .parse()
            .map_err(|e| format!("invalid probe URL {target:?}: {e}"))?;
        let host = if self.cfg.host.is_empty() {
            &*ep.address
        } else {
            &self.cfg.host
        };
        let host = HeaderValue::from_str(host)
            .map_err(|_| format!("invalid probe Host header {host:?}"))?;
        let mut req = Request::new(Empty::new());
        *req.method_mut() = Method::GET;
        *req.uri_mut() = uri;
        let headers = req.headers_mut();
        headers.insert(HOST, host);
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        headers.insert(CONNECTION, HeaderValue::from_static("close"));

        let exchange = async {
            let resp = self.client.send(req).await.map_err(describe)?;
            let status = resp.status();
            let mut body = resp.into_body();
            let mut read = 0;
            // Like steer, a body that fails part way is no failure: the
            // status has already said everything.
            while read < MAX_BODY_READ {
                match body.frame().await {
                    Some(Ok(frame)) => {
                        read += frame.data_ref().map_or(0, Buf::remaining);
                    }
                    Some(Err(_)) | None => break,
                }
            }
            if status.is_success() {
                Ok(())
            } else {
                Err(format!("probe returned status {}", status.as_u16()))
            }
        };
        match tokio::time::timeout(self.cfg.timeout, exchange).await {
            Ok(result) => result,
            Err(_) => Err(describe(ClientError::Timeout)),
        }
    }
}

async fn until_stopped(stopped: &mut watch::Receiver<bool>) {
    // A dropped sender also stops the loop.
    let _ = stopped.wait_for(|s| *s).await;
}

fn describe(e: ClientError) -> String {
    let what = match e {
        ClientError::Connect => "connection failed",
        ClientError::ConnectTimeout => "connect timed out",
        ClientError::Timeout => "timed out",
        ClientError::HeadersTooLarge => "response headers too large",
        ClientError::Protocol => "malformed response",
    };
    format!("probe failed: {what}")
}

/// A uniform draw from [0, 1), like Go's `rand.Float64`.
pub(crate) fn unit(x: u64) -> f64 {
    (x >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_draws_stay_below_one() {
        assert_eq!(unit(0), 0.0);
        assert!(unit(u64::MAX) < 1.0);
        assert_eq!(unit(1 << 63), 0.5);
    }

    #[test]
    fn every_client_error_has_words() {
        for e in [
            ClientError::Connect,
            ClientError::ConnectTimeout,
            ClientError::Timeout,
            ClientError::HeadersTooLarge,
            ClientError::Protocol,
        ] {
            let s = describe(e);
            assert!(s.starts_with("probe failed: ") && s.len() > 15, "{s}");
        }
    }
}
