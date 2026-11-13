//! Passive outlier detection, ported from steer's `internal/outlier`. It
//! ejects endpoints for the responses they actually return, following
//! Envoy's outlier detection.
//!
//! Two detectors run per cluster:
//!
//! - **Consecutive gateway failures.** An endpoint that returns
//!   `consecutive_gateway_failures` connect errors, resets, timeouts or
//!   502/503/504 in a row is ejected at once, from [`Detector::report`].
//! - **Success rate.** Every interval, [`Detector::tick`] looks at the
//!   endpoints that served at least `success_rate_request_volume` attempts
//!   in the interval that just ended. If there are at least
//!   `success_rate_minimum_hosts` of them, any whose success rate is below
//!   `mean - success_rate_stdev_factor * stdev` is ejected, worst first.
//!   When every rate is the same nothing is an outlier.
//!
//! An ejection lasts `base_ejection_time` times the number of times the
//! endpoint has been ejected, capped at `max_ejection_time`. The multiplier
//! drops by one for each full base period the endpoint then stays
//! un-ejected, checked on every tick. No more than `max_ejection_percent`
//! of the cluster is ejected at once, but when the percent is above zero
//! one endpoint may always be ejected; zero turns ejection off.
//!
//! The counters, the multiplier and the ejection itself live in each
//! endpoint's [`State`], where the balancer reads them without a lock. The
//! detector only decides: a mutex serialises its decisions so that the cap
//! holds under concurrent reports.
//!
//! One difference from steer, which config validation makes unreachable:
//! an endpoint with no attempts in the interval never counts towards the
//! success rate, even with a request volume of zero. steer would divide
//! zero by zero there and eject on the resulting NaN.
//!
//! [`State`]: crate::endpoint::State

use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use edgefront_core::clock::Clock;
use edgefront_core::config::OutlierDetection;

use crate::endpoint::Endpoint;

/// The reason given for an ejection by the consecutive-failure detector,
/// as steer logs and labels it.
pub const REASON_CONSECUTIVE_GATEWAY_FAILURE: &str = "consecutive_gateway_failure";
/// The reason given for an ejection by the success-rate detector.
pub const REASON_SUCCESS_RATE: &str = "success_rate";

/// Told about every ejection: the endpoint, the reason and when the
/// ejection ends.
pub type EjectFn = Box<dyn Fn(&Endpoint, &str, Instant) + Send + Sync>;

/// Runs outlier detection for one cluster. It is safe to share between
/// threads.
pub struct Detector {
    cfg: OutlierDetection,
    eps: Vec<Endpoint>,
    clock: Arc<dyn Clock>,
    on_eject: Option<EjectFn>,
    /// Serialises ejection decisions so the cap holds. It also keeps the
    /// success-rate samples, so a tick reuses one allocation.
    samples: Mutex<Vec<Sample>>,
}

/// One endpoint's success rate over an interval: its index and the rate.
#[derive(Debug, Clone, Copy)]
struct Sample {
    index: usize,
    rate: f64,
}

impl fmt::Debug for Detector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Detector")
            .field("cfg", &self.cfg)
            .field("endpoints", &self.eps.len())
            .finish_non_exhaustive()
    }
}

impl Detector {
    /// A detector over `eps`, timed on `clock`, telling `on_eject` about
    /// each ejection.
    pub fn new(
        cfg: OutlierDetection,
        eps: Vec<Endpoint>,
        clock: Arc<dyn Clock>,
        on_eject: Option<EjectFn>,
    ) -> Detector {
        let samples = Mutex::new(Vec::with_capacity(eps.len()));
        Detector {
            cfg,
            eps,
            clock,
            on_eject,
            samples,
        }
    }

    /// The configuration it runs with.
    pub fn config(&self) -> &OutlierDetection {
        &self.cfg
    }

    /// The endpoints it watches.
    pub fn endpoints(&self) -> &[Endpoint] {
        &self.eps
    }

    /// Records the outcome of one attempt against `ep`. A gateway failure
    /// is a connect error, reset, timeout or 502, 503 or 504; a failure is
    /// one of those or any other 5xx. Ejects `ep` when this completes a run
    /// of gateway failures and the cap allows it.
    pub fn report(&self, ep: &Endpoint, gateway_failure: bool, failure: bool) {
        let run = ep.state.record_result(gateway_failure, failure);
        let needed = self.cfg.consecutive_gateway_failures;
        if needed == 0 || run < needed {
            return;
        }
        let _decide = self.lock();
        let now = self.clock.now();
        if !ep.state.ejected(now) && self.can_eject(now) {
            self.eject(ep, now, REASON_CONSECUTIVE_GATEWAY_FAILURE);
        }
    }

    /// The per-interval work: decays the ejection multipliers, then judges
    /// the success rates of the interval that just ended and starts a new
    /// one.
    pub fn tick(&self) {
        let mut samples = self.lock();
        samples.clear();
        let now = self.clock.now();
        let enabled = self.cfg.success_rate_minimum_hosts > 0;
        let volume = u64::from(self.cfg.success_rate_request_volume).max(1);
        for (index, ep) in self.eps.iter().enumerate() {
            ep.state.decay(now, self.cfg.base_ejection_time);
            let (ok, failed) = ep.state.take_interval();
            if !enabled || ep.state.ejected(now) {
                continue;
            }
            let total = ok + failed;
            if total >= volume {
                samples.push(Sample {
                    index,
                    rate: ok as f64 / total as f64,
                });
            }
        }
        if !enabled || samples.len() < self.cfg.success_rate_minimum_hosts as usize {
            return;
        }
        let n = samples.len() as f64;
        let mean = samples.iter().map(|s| s.rate).sum::<f64>() / n;
        let squares: f64 = samples
            .iter()
            .map(|s| (s.rate - mean) * (s.rate - mean))
            .sum();
        let stdev = (squares / n).sqrt();
        if stdev < 1e-9 {
            // Everyone is equally good (or bad): nothing is an outlier.
            return;
        }
        let threshold = mean - self.cfg.success_rate_stdev_factor * stdev;
        // Worst first, so the cap removes the clearest outliers; a stable
        // sort keeps equal rates in endpoint order, as steer does.
        samples.sort_by(|a, b| a.rate.total_cmp(&b.rate));
        for s in samples.iter() {
            if s.rate >= threshold {
                break;
            }
            if !self.can_eject(now) {
                return;
            }
            self.eject(&self.eps[s.index], now, REASON_SUCCESS_RATE);
        }
    }

    /// Calls [`Detector::tick`] every interval on the clock until
    /// `shutdown` resolves.
    pub async fn run(&self, shutdown: impl Future<Output = ()>) {
        tokio::pin!(shutdown);
        loop {
            let timer = self.clock.after(self.cfg.interval);
            tokio::select! {
                biased;
                () = &mut shutdown => return,
                _ = timer => self.tick(),
            }
        }
    }

    /// Whether `max_ejection_percent` allows one more ejection at `now`.
    /// Callers hold the decision lock.
    fn can_eject(&self, now: Instant) -> bool {
        let percent = usize::from(self.cfg.max_ejection_percent);
        let mut limit = self.eps.len() * percent / 100;
        if percent > 0 && limit < 1 {
            limit = 1;
        }
        let ejected = self.eps.iter().filter(|ep| ep.state.ejected(now)).count();
        ejected < limit
    }

    fn eject(&self, ep: &Endpoint, now: Instant, reason: &str) {
        let until = ep.state.eject(
            now,
            self.cfg.base_ejection_time,
            self.cfg.max_ejection_time,
            reason,
        );
        if let Some(f) = &self.on_eject {
            f(ep, reason, until);
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Sample>> {
        // The samples are cleared before every use, so a panic that left
        // them half-written leaves nothing to repair.
        self.samples.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
