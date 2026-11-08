//! The live state of one upstream address, ported from steer's
//! `internal/endpoint`: requests in flight, the peak-EWMA latency estimate,
//! the active health verdict and the passive outlier counters.
//!
//! State is kept apart from configuration, behind an `Arc`, so a reload
//! that keeps an address keeps everything learned about it.
//!
//! Everything a pick reads is an atomic and is read without a lock: the
//! in-flight count, the health verdict, the end of the current ejection and
//! the latency estimate. The health check and outlier counters change
//! together, so they sit behind one mutex, as in steer; only health checks,
//! response accounting and the admin API take it.
//!
//! Times are nanoseconds since an epoch fixed when the state is made, the
//! way steer keeps Unix nanoseconds. Every instant given to one state
//! should come from the same [`Clock`](edgefront_core::clock::Clock); one
//! before the epoch counts as the epoch.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::balancer::ewma::{self, Cost, PeakEwma};

pub use crate::balancer::ewma::LATENCY_DECAY;

/// One configured upstream address and its shared state, steer's
/// `endpoint.Endpoint`.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// The upstream's `host:port`.
    pub address: Box<str>,
    /// Its weight within its priority level.
    pub weight: u32,
    /// Its priority level; 0 is the most preferred.
    pub priority: u32,
    /// The state that survives reloads.
    pub state: Arc<State>,
}

impl Endpoint {
    /// An endpoint with fresh state timed from `epoch`.
    pub fn new(address: &str, weight: u32, priority: u32, epoch: Instant) -> Endpoint {
        Endpoint::with_state(address, weight, priority, Arc::new(State::new(epoch)))
    }

    /// An endpoint that carries on with existing state, for a reload that
    /// keeps the address.
    pub fn with_state(address: &str, weight: u32, priority: u32, state: Arc<State>) -> Endpoint {
        Endpoint {
            address: address.into(),
            weight,
            priority,
            state,
        }
    }
}

impl Cost for Endpoint {
    fn cost(&self, now: Instant) -> f64 {
        self.state.cost(now)
    }
}

/// The mutable, reload-surviving part of an endpoint, steer's
/// `endpoint.State`.
pub struct State {
    epoch: Instant,
    in_flight: AtomicI64,
    /// The active health check verdict.
    healthy: AtomicBool,
    /// When the current or last ejection ends, as nanoseconds since the
    /// epoch plus one; zero when there has been none since the last clear.
    ejected_until: AtomicU64,
    latency: PeakEwma,
    counters: Mutex<Counters>,
}

/// What changes together and is read off the request path.
#[derive(Debug, Default)]
struct Counters {
    // Active health checking.
    hc_successes: u32,
    hc_failures: u32,
    last_probe_error: String,
    // Passive outlier detection.
    consecutive_gateway: u32,
    /// Attempts since the last interval.
    successes: u64,
    failures: u64,
    times_ejected: u64,
    last_eject_reason: String,
    /// Nanoseconds since the epoch from which un-ejected time counts
    /// towards lowering `times_ejected`.
    decay_from: u64,
}

impl State {
    /// State timed from `epoch`, for an endpoint that is assumed healthy
    /// until a health check says otherwise.
    pub fn new(epoch: Instant) -> State {
        State {
            epoch,
            in_flight: AtomicI64::new(0),
            healthy: AtomicBool::new(true),
            ejected_until: AtomicU64::new(0),
            latency: PeakEwma::new(epoch),
            counters: Mutex::new(Counters::default()),
        }
    }

    fn nanos(&self, t: Instant) -> u64 {
        // u64 nanoseconds last 584 years.
        u64::try_from(t.saturating_duration_since(self.epoch).as_nanos()).unwrap_or(u64::MAX)
    }

    fn instant(&self, nanos: u64) -> Instant {
        // Saturates where `Instant` cannot go that far.
        let mut d = Duration::from_nanos(nanos);
        loop {
            if let Some(t) = self.epoch.checked_add(d) {
                return t;
            }
            d /= 2;
        }
    }

    fn lock(&self) -> MutexGuard<'_, Counters> {
        // Every update leaves the counters consistent, so a panic elsewhere
        // while holding the lock cannot leave them half-written.
        self.counters.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the start of a request to the endpoint.
    pub fn acquire(&self) {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
    }

    /// Records the end of a request started with [`State::acquire`].
    pub fn release(&self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    /// Records the start of a request and returns a guard that records its
    /// end when dropped, so an early return or a panic cannot leak it.
    pub fn track(self: &Arc<Self>) -> InFlight {
        self.acquire();
        InFlight(Arc::clone(self))
    }

    /// The number of requests outstanding.
    pub fn in_flight(&self) -> i64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Folds one response time, observed at `now`, into the peak-EWMA
    /// estimate: a slower sample replaces it at once, a faster one pulls it
    /// down with time constant [`LATENCY_DECAY`].
    pub fn observe_latency(&self, sample: Duration, now: Instant) {
        self.latency.observe(sample, now);
    }

    /// The latency estimate in seconds as of `now`; zero before any sample.
    /// Between samples it decays towards zero.
    pub fn latency(&self, now: Instant) -> f64 {
        self.latency.latency(now)
    }

    /// The least-loaded score: the latency estimate, at least one
    /// microsecond, times the requests in flight plus one.
    pub fn cost(&self, now: Instant) -> f64 {
        ewma::cost(self.latency(now), self.in_flight())
    }

    /// The active health check verdict.
    pub fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    /// Overrides the verdict and forgets the probe runs, for when health
    /// checking is turned off.
    pub fn set_healthy(&self, healthy: bool) {
        let mut c = self.lock();
        self.healthy.store(healthy, Ordering::Release);
        c.hc_successes = 0;
        c.hc_failures = 0;
        c.last_probe_error.clear();
    }

    /// Whether outlier detection has the endpoint ejected at `now`.
    pub fn ejected(&self, now: Instant) -> bool {
        // Stored as nanoseconds plus one, so `now + 1 < stored` is
        // `now < until`, and zero is never ejected.
        self.nanos(now).saturating_add(1) < self.ejected_until.load(Ordering::Acquire)
    }

    /// When the current or last ejection ends; `None` when there has been
    /// none since the state was made or the ejection cleared.
    pub fn ejected_until(&self) -> Option<Instant> {
        match self.ejected_until.load(Ordering::Acquire) {
            0 => None,
            n => Some(self.instant(n - 1)),
        }
    }

    /// Whether the load balancer may send requests here at `now`: healthy
    /// and not ejected.
    pub fn available(&self, now: Instant) -> bool {
        self.healthy() && !self.ejected(now)
    }

    /// Applies one health check result. The verdict turns healthy after
    /// `healthy_after` passes in a row and unhealthy after
    /// `unhealthy_after` failures in a row. Returns the verdict and whether
    /// it changed.
    pub fn record_probe(
        &self,
        ok: bool,
        err: &str,
        healthy_after: u32,
        unhealthy_after: u32,
    ) -> (bool, bool) {
        let mut c = self.lock();
        let was = self.healthy.load(Ordering::Relaxed);
        if ok {
            c.hc_failures = 0;
            c.hc_successes = c.hc_successes.saturating_add(1);
            c.last_probe_error.clear();
            if !was && c.hc_successes >= healthy_after {
                self.healthy.store(true, Ordering::Release);
            }
        } else {
            c.hc_successes = 0;
            c.hc_failures = c.hc_failures.saturating_add(1);
            err.clone_into(&mut c.last_probe_error);
            if was && c.hc_failures >= unhealthy_after {
                self.healthy.store(false, Ordering::Release);
            }
        }
        let now = self.healthy.load(Ordering::Relaxed);
        (now, now != was)
    }

    /// Counts one upstream attempt. A gateway failure is a connect error,
    /// reset, timeout or 502, 503 or 504; a failure is one of those or any
    /// other 5xx. Returns the current run of gateway failures in a row.
    pub fn record_result(&self, gateway_failure: bool, failure: bool) -> u32 {
        let mut c = self.lock();
        if gateway_failure {
            c.consecutive_gateway = c.consecutive_gateway.saturating_add(1);
        } else {
            c.consecutive_gateway = 0;
        }
        if failure || gateway_failure {
            c.failures += 1;
        } else {
            c.successes += 1;
        }
        c.consecutive_gateway
    }

    /// Returns and resets the attempt counts of the current interval, as
    /// (successes, failures).
    pub fn take_interval(&self) -> (u64, u64) {
        let mut c = self.lock();
        (
            std::mem::take(&mut c.successes),
            std::mem::take(&mut c.failures),
        )
    }

    /// Ejects the endpoint at `now` for `base` times the number of times it
    /// has been ejected, capped at `max`, and returns when the ejection
    /// ends. A zero `base` ejects for no time.
    pub fn eject(&self, now: Instant, base: Duration, max: Duration, reason: &str) -> Instant {
        let mut c = self.lock();
        c.times_ejected = c.times_ejected.saturating_add(1);
        let d = ejection_nanos(c.times_ejected, base, max);
        let until = self.nanos(now).saturating_add(d);
        self.ejected_until
            .store(until.saturating_add(1), Ordering::Release);
        c.consecutive_gateway = 0;
        reason.clone_into(&mut c.last_eject_reason);
        c.decay_from = until;
        self.instant(until)
    }

    /// Ends any ejection and forgets the multiplier, for when outlier
    /// detection is turned off for the endpoint's cluster.
    pub fn clear_ejection(&self) {
        let mut c = self.lock();
        self.ejected_until.store(0, Ordering::Release);
        c.times_ejected = 0;
        c.consecutive_gateway = 0;
    }

    /// Lowers the ejection multiplier by one for each full `period` the
    /// endpoint has spent un-ejected since its last ejection ended, so a
    /// host that has recovered starts again from the base ejection time. A
    /// part period carries over to the next call.
    pub fn decay(&self, now: Instant, period: Duration) {
        let mut c = self.lock();
        if c.times_ejected == 0 || self.ejected(now) {
            return;
        }
        let since = self.nanos(now).saturating_sub(c.decay_from);
        let period = u64::try_from(period.as_nanos()).unwrap_or(u64::MAX);
        // steer steps one period at a time; this takes them all at once.
        // A zero period decays fully, as steer's loop does.
        let steps = since.checked_div(period).unwrap_or(u64::MAX);
        let steps = steps.min(c.times_ejected);
        c.times_ejected -= steps;
        c.decay_from = c.decay_from.saturating_add(steps.saturating_mul(period));
    }

    /// The current ejection multiplier.
    pub fn times_ejected(&self) -> u64 {
        self.lock().times_ejected
    }

    /// A point-in-time copy of the state as of `now`, for the admin API.
    pub fn snapshot(&self, now: Instant) -> Snapshot {
        let c = self.lock();
        let healthy = self.healthy();
        let ejected = self.ejected(now);
        Snapshot {
            healthy,
            last_probe_error: c.last_probe_error.clone(),
            ejected,
            ejected_until: self.ejected_until(),
            times_ejected: c.times_ejected,
            last_eject_reason: c.last_eject_reason.clone(),
            consecutive_gateway_failures: c.consecutive_gateway,
            in_flight: self.in_flight(),
            latency_ms: (self.latency(now) * 1e6).round() / 1e3,
            available: healthy && !ejected,
        }
    }
}

/// How long the `times`-th ejection lasts, in nanoseconds: steer's
/// `min(min(times, max/base + 1) * base, max)`, which cannot overflow
/// however many ejections pile up.
fn ejection_nanos(times: u64, base: Duration, max: Duration) -> u64 {
    let (base, max) = (base.as_nanos(), max.as_nanos());
    let Some(cap) = max.checked_div(base) else {
        return 0;
    };
    let mult = u128::from(times).min(cap + 1);
    // mult * base <= max + base, far inside u128.
    u64::try_from((mult * base).min(max)).unwrap_or(u64::MAX)
}

impl Cost for State {
    fn cost(&self, now: Instant) -> f64 {
        State::cost(self, now)
    }
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("State")
            .field("in_flight", &self.in_flight())
            .field("healthy", &self.healthy())
            .field("ejected_until", &self.ejected_until())
            .field("latency", &self.latency)
            .finish_non_exhaustive()
    }
}

/// A request in flight to an endpoint, from [`State::track`]; dropping it
/// records the request's end.
#[derive(Debug)]
#[must_use = "dropping the guard ends the request at once"]
pub struct InFlight(Arc<State>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// A point-in-time copy of an endpoint's state, steer's
/// `endpoint.Snapshot`.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// The active health check verdict.
    pub healthy: bool,
    /// The last failed probe's error; empty after a pass.
    pub last_probe_error: String,
    /// Whether the endpoint is ejected now.
    pub ejected: bool,
    /// When the current or last ejection ends.
    pub ejected_until: Option<Instant>,
    /// The ejection multiplier.
    pub times_ejected: u64,
    /// Why the endpoint was last ejected.
    pub last_eject_reason: String,
    /// The current run of gateway failures in a row.
    pub consecutive_gateway_failures: u32,
    /// Requests outstanding.
    pub in_flight: i64,
    /// The latency estimate in milliseconds, rounded to the microsecond.
    pub latency_ms: f64,
    /// Whether the load balancer may pick the endpoint now.
    pub available: bool,
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;

    use super::*;

    const MS: Duration = Duration::from_millis(1);
    const SEC: Duration = Duration::from_secs(1);

    fn epoch() -> Instant {
        Instant::now()
    }

    // TestPeakEWMA.
    #[test]
    fn peak_ewma() {
        let t0 = epoch();
        let s = Arc::new(State::new(t0));
        assert_eq!(s.latency(t0), 0.0);
        assert_eq!(s.cost(t0), 1e-6);
        s.observe_latency(10 * MS, t0);
        assert_eq!(s.latency(t0), 0.01);
        let t1 = t0 + MS;
        s.observe_latency(50 * MS, t1); // slower: jumps to the peak
        assert_eq!(s.latency(t1), 0.05);
        // A faster sample one time constant later moves 1-1/e of the way.
        let t2 = t1 + LATENCY_DECAY;
        s.observe_latency(10 * MS, t2);
        let e = std::f64::consts::E;
        let want = 0.05 / e + 0.01 * (1.0 - 1.0 / e);
        assert!((s.latency(t2) - want).abs() < 1e-9);
        // Left alone, the estimate decays towards zero.
        assert!((s.latency(t2 + LATENCY_DECAY) - want / e).abs() < 1e-9);
        // Reading at an earlier time does not inflate the estimate.
        assert!(s.latency(t0) <= s.latency(t2));
        s.acquire();
        let guard = s.track();
        assert_eq!(s.in_flight(), 2);
        assert!((s.cost(t2) - 3.0 * s.latency(t2)).abs() < 1e-12);
        assert_eq!(Cost::cost(&*s, t2), s.cost(t2));
        drop(guard);
        s.release();
        assert_eq!(s.in_flight(), 0);
    }

    // TestRecordProbeThresholds.
    #[test]
    fn record_probe_thresholds() {
        let t0 = epoch();
        let s = State::new(t0);
        assert_eq!(s.record_probe(false, "x", 2, 2), (true, false));
        assert_eq!(s.record_probe(false, "refused", 2, 2), (false, true));
        let snap = s.snapshot(t0);
        assert_eq!(snap.last_probe_error, "refused");
        assert!(!snap.available);
        assert_eq!(s.record_probe(false, "x", 2, 2), (false, false));
        assert_eq!(s.record_probe(true, "", 2, 2), (false, false));
        assert_eq!(s.snapshot(t0).last_probe_error, "");
        assert_eq!(s.record_probe(true, "", 2, 2), (true, true));
        s.record_probe(false, "x", 2, 2);
        s.set_healthy(false);
        assert!(!s.healthy());
        assert_eq!(s.snapshot(t0).last_probe_error, "");
        // set_healthy forgot the failure run: one more pass is not enough
        // to count as two.
        assert_eq!(s.record_probe(true, "", 2, 2), (false, false));
        assert_eq!(s.record_probe(true, "", 2, 2), (true, true));
    }

    // TestEjectionLifecycle.
    #[test]
    fn ejection_lifecycle() {
        let t0 = epoch();
        let s = State::new(t0);
        assert!(!s.ejected(t0));
        assert_eq!(s.ejected_until(), None);
        for _ in 0..3 {
            s.record_result(true, true);
        }
        let until = s.eject(t0, 10 * SEC, 25 * SEC, "test");
        assert_eq!(until, t0 + 10 * SEC);
        assert!(s.ejected(t0));
        assert!(!s.available(t0));
        assert!(s.ejected(until - Duration::from_nanos(1)));
        assert!(!s.ejected(until));
        let snap = s.snapshot(t0);
        assert_eq!(snap.consecutive_gateway_failures, 0);
        assert_eq!(snap.last_eject_reason, "test");
        assert_eq!(snap.times_ejected, 1);
        assert!(snap.ejected);
        assert_eq!(s.ejected_until(), Some(until));
        // Decay does nothing while ejected.
        s.decay(t0 + 5 * SEC, 10 * SEC);
        assert_eq!(s.times_ejected(), 1);
        let until = s.eject(t0 + 10 * SEC, 10 * SEC, 25 * SEC, "test");
        assert_eq!(until - (t0 + 10 * SEC), 20 * SEC);
        let until = s.eject(t0 + 30 * SEC, 10 * SEC, 25 * SEC, "test");
        assert_eq!(until - (t0 + 30 * SEC), 25 * SEC);
        s.clear_ejection();
        assert!(!s.ejected(t0 + 30 * SEC));
        assert_eq!(s.times_ejected(), 0);
        assert_eq!(s.ejected_until(), None);
        // The multiplier cannot overflow, however many ejections pile up.
        for _ in 0..1000 {
            s.eject(t0, Duration::from_nanos(1), Duration::from_secs(3600), "x");
        }
        assert!(s.ejected_until().unwrap() > t0);
        assert_eq!(s.times_ejected(), 1000);
    }

    #[test]
    fn ejection_edges() {
        let t0 = epoch();
        let s = State::new(t0);
        // A zero base ejects for no time but still records the ejection,
        // where steer would divide by zero.
        let until = s.eject(t0, Duration::ZERO, SEC, "zero");
        assert_eq!(until, t0);
        assert!(!s.ejected(t0));
        assert_eq!(s.ejected_until(), Some(t0));
        assert_eq!(s.times_ejected(), 1);
        // An instant before the epoch counts as the epoch.
        let late = State::new(t0 + SEC);
        assert_eq!(late.eject(t0, SEC, SEC, "early"), t0 + 2 * SEC);
        assert!(late.ejected(t0));
        // A huge ejection saturates instead of overflowing.
        let big = State::new(t0);
        let until = big.eject(t0, Duration::MAX, Duration::MAX, "big");
        assert!(until > t0 + Duration::from_secs(1 << 30));
        assert!(big.ejected(until - SEC));
        assert_eq!(
            ejection_nanos(u64::MAX, Duration::MAX, Duration::MAX),
            u64::MAX
        );
        assert_eq!(ejection_nanos(3, 10 * SEC, 25 * SEC), 25_000_000_000);
        assert_eq!(ejection_nanos(2, 10 * SEC, 25 * SEC), 20_000_000_000);
        assert_eq!(ejection_nanos(9, SEC, Duration::ZERO), 0);
    }

    // TestDecay.
    #[test]
    fn decay() {
        let period = 10 * SEC;
        let t0 = epoch();
        let s = State::new(t0);
        s.decay(t0, period); // never ejected: nothing to do
        assert_eq!(s.times_ejected(), 0);
        for _ in 0..3 {
            s.eject(t0, SEC, Duration::from_secs(3600), "x"); // multiplier 3, ends at t0+3s
        }
        let end = t0 + 3 * SEC;
        s.decay(end + period - Duration::from_nanos(1), period);
        assert_eq!(s.times_ejected(), 3, "a part period decayed");
        s.decay(end + period, period);
        assert_eq!(s.times_ejected(), 2);
        // The unused remainder of a period still counts at the next call.
        s.decay(end + 2 * period + period / 2, period);
        assert_eq!(s.times_ejected(), 1);
        s.decay(end + 3 * period, period);
        assert_eq!(s.times_ejected(), 0);
        s.decay(end + 100 * period, period); // never below zero
        assert_eq!(s.times_ejected(), 0);
        // Several periods at once decay several steps, and a zero period
        // decays fully.
        for _ in 0..5 {
            s.eject(t0, SEC, Duration::from_secs(3600), "x");
        }
        let end = t0 + 5 * SEC;
        s.decay(end + 3 * period, period);
        assert_eq!(s.times_ejected(), 2);
        s.decay(end + 3 * period, Duration::ZERO);
        assert_eq!(s.times_ejected(), 0);
    }

    // TestIntervalCounters.
    #[test]
    fn interval_counters() {
        let s = State::new(epoch());
        assert_eq!(s.record_result(false, false), 0);
        assert_eq!(s.record_result(false, true), 0);
        assert_eq!(s.record_result(true, true), 1);
        assert_eq!(s.record_result(true, false), 2);
        assert_eq!(s.take_interval(), (1, 3));
        assert_eq!(s.take_interval(), (0, 0));
    }

    #[test]
    fn endpoint_shares_state_across_reloads() {
        let t0 = epoch();
        let a = Endpoint::new("a:1", 2, 3, t0);
        assert_eq!((&*a.address, a.weight, a.priority), ("a:1", 2, 3));
        a.state.observe_latency(5 * MS, t0);
        let _req = a.state.track();
        let reloaded = Endpoint::with_state("a:1", 5, 0, Arc::clone(&a.state));
        assert_eq!(reloaded.state.in_flight(), 1);
        assert_eq!(reloaded.cost(t0), 0.005 * 2.0);
        let snap = reloaded.state.snapshot(t0);
        assert_eq!(snap.latency_ms, 5.0);
        assert!(snap.available && snap.healthy && !snap.ejected);
        assert_eq!(snap.ejected_until, None);
        let dbg = format!("{a:?}");
        assert!(
            dbg.contains("in_flight: 1") && dbg.contains("healthy: true"),
            "{dbg}"
        );
    }

    #[test]
    fn survives_a_poisoned_lock() {
        let s = Arc::new(State::new(epoch()));
        let s2 = Arc::clone(&s);
        let _ = thread::spawn(move || {
            let _guard = s2.counters.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert_eq!(s.record_result(true, true), 1);
        assert_eq!(s.take_interval(), (0, 1));
    }

    #[test]
    fn instant_saturates() {
        let s = State::new(epoch());
        assert!(s.instant(u64::MAX) >= s.epoch);
        assert_eq!(s.instant(0), s.epoch);
    }

    // TestConcurrentUse, at 16 threads, checking every update is counted.
    #[test]
    fn sixteen_threads_lose_no_updates() {
        const THREADS: u64 = 16;
        const ROUNDS: u64 = if cfg!(miri) { 50 } else { 5_000 };
        let t0 = epoch();
        let s = Arc::new(State::new(t0));
        let start = Arc::new(Barrier::new(THREADS as usize + 1));
        let workers: Vec<_> = (0..THREADS)
            .map(|id| {
                let s = Arc::clone(&s);
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    start.wait();
                    let (mut ok, mut bad, mut taken) = (0u64, 0u64, (0u64, 0u64));
                    let mut peak = 0;
                    for k in 0..ROUNDS {
                        let guard = s.track();
                        s.acquire();
                        peak = peak.max(s.in_flight());
                        let at = t0 + Duration::from_micros(k);
                        s.observe_latency(MS + Duration::from_micros(id * ROUNDS + k), at);
                        let (gw, fail) = (k % 3 == 0, k % 5 == 0);
                        if gw || fail {
                            bad += 1;
                        } else {
                            ok += 1;
                        }
                        s.record_result(gw, fail);
                        if k % 97 == id {
                            let (a, b) = s.take_interval();
                            taken.0 += a;
                            taken.1 += b;
                        }
                        s.record_probe(k % 7 != 0, "flap", 2, 3);
                        let snap = s.snapshot(at);
                        assert!(snap.in_flight >= 1 && snap.latency_ms > 0.0);
                        s.release();
                        drop(guard);
                    }
                    (ok, bad, taken, peak)
                })
            })
            .collect();
        start.wait();
        let (mut ok, mut bad, mut taken) = (0, 0, (0, 0));
        let mut peak = 0;
        for w in workers {
            let (o, b, t, p) = w.join().unwrap();
            ok += o;
            bad += b;
            taken.0 += t.0;
            taken.1 += t.1;
            peak = peak.max(p);
        }
        assert_eq!(s.in_flight(), 0, "lost an acquire or release");
        assert!((2..=2 * THREADS as i64).contains(&peak));
        let rest = s.take_interval();
        assert_eq!(taken.0 + rest.0, ok, "lost a success");
        assert_eq!(taken.1 + rest.1, bad, "lost a failure");
        assert_eq!(ok + bad, THREADS * ROUNDS);
        // Every sample is a blend of real samples: the estimate is never
        // above the slowest of all.
        let slowest = (MS + Duration::from_micros(THREADS * ROUNDS - 1)).as_secs_f64();
        let latency = s.latency(t0);
        assert!(latency > 0.0 && latency <= slowest, "{latency}");
    }

    #[test]
    fn concurrent_ejections_count_every_one() {
        const THREADS: u64 = 16;
        const EACH: u64 = 500;
        let t0 = epoch();
        let s = Arc::new(State::new(t0));
        let workers: Vec<_> = (0..THREADS)
            .map(|_| {
                let s = Arc::clone(&s);
                thread::spawn(move || {
                    for _ in 0..EACH {
                        s.eject(t0, MS, Duration::from_secs(3600), "x");
                        assert!(s.ejected(t0));
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        assert_eq!(s.times_ejected(), THREADS * EACH);
        // The last ejection made lasts the full multiplier.
        assert_eq!(
            s.ejected_until(),
            Some(t0 + MS * u32::try_from(THREADS * EACH).unwrap())
        );
    }
}
