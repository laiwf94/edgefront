//! Power of two choices on peak-EWMA cost, ported from steer's
//! `balancer.LeastLoaded` and the latency estimate of `endpoint.State`.
//!
//! [`LeastLoaded`] picks two distinct candidates at random and keeps the one
//! with the lower [`Cost`]: in-flight requests (plus one) times a peak-EWMA
//! latency estimate. This is Mitzenmacher's power of two choices, as Finagle
//! and Linkerd use it with peak EWMA. It avoids the herding of "always pick
//! the least loaded" when many proxies share the same stale view, and costs
//! O(1) per pick on the common path.
//!
//! [`PeakEwma`] is the estimate: a slower sample replaces it at once, a
//! faster one pulls it down with time constant [`LATENCY_DECAY`], and
//! between samples it decays towards zero, so an endpoint that was penalised
//! and then left alone is eventually tried, and measured, again.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use edgefront_core::clock::Clock;

use super::Policy;
use super::rng::Rng;

/// The time constant of the peak-EWMA latency estimate.
pub const LATENCY_DECAY: Duration = Duration::from_secs(10);

/// The latency an endpoint with no estimate scores as, in seconds, so new
/// endpoints receive traffic and get measured.
pub const MIN_LATENCY: f64 = 1e-6;

/// What power of two choices compares: lower is better.
pub trait Cost {
    /// The endpoint's cost as of `now`.
    fn cost(&self, now: Instant) -> f64;
}

impl<T: Cost + ?Sized> Cost for Arc<T> {
    fn cost(&self, now: Instant) -> f64 {
        (**self).cost(now)
    }
}

/// steer's least-loaded score: the latency estimate in seconds, at least
/// [`MIN_LATENCY`], times the requests in flight plus one.
pub fn cost(latency: f64, in_flight: i64) -> f64 {
    latency.max(MIN_LATENCY) * (in_flight + 1) as f64
}

/// A peak-EWMA latency estimate. Reads are lock-free; samples are
/// serialized.
///
/// Times are kept as nanoseconds since an epoch fixed at construction, the
/// way steer keeps Unix nanoseconds. Every instant given to one estimate
/// should come from the same clock; one before the epoch counts as the
/// epoch.
pub struct PeakEwma {
    epoch: Instant,
    /// The estimate in seconds as of `at`, as `f64` bits; zero before the
    /// first sample.
    bits: AtomicU64,
    /// Nanoseconds since the epoch.
    at: AtomicU64,
    /// Whether a sample has been seen, and the lock samples take.
    observed: Mutex<bool>,
}

impl PeakEwma {
    /// An estimate with no samples, timed from `epoch`.
    pub fn new(epoch: Instant) -> PeakEwma {
        PeakEwma {
            epoch,
            bits: AtomicU64::new(0),
            at: AtomicU64::new(0),
            observed: Mutex::new(false),
        }
    }

    fn nanos(&self, t: Instant) -> u64 {
        // u64 nanoseconds last 584 years.
        u64::try_from(t.saturating_duration_since(self.epoch).as_nanos()).unwrap_or(u64::MAX)
    }

    /// Folds one response time, observed at `now`, into the estimate.
    pub fn observe(&self, sample: Duration, now: Instant) {
        let sample = sample.as_secs_f64();
        let now = self.nanos(now);
        let mut observed = self.observed.lock().unwrap_or_else(PoisonError::into_inner);
        let mut cur = f64::from_bits(self.bits.load(Ordering::Relaxed));
        if !*observed || sample > cur {
            cur = sample;
        } else {
            let w = decay_weight(now.saturating_sub(self.at.load(Ordering::Relaxed)));
            cur = cur * w + sample * (1.0 - w);
        }
        *observed = true;
        self.at.store(now, Ordering::Relaxed);
        self.bits.store(cur.to_bits(), Ordering::Relaxed);
    }

    /// The estimate in seconds as of `now`; zero before any sample.
    pub fn latency(&self, now: Instant) -> f64 {
        let cur = f64::from_bits(self.bits.load(Ordering::Relaxed));
        if cur == 0.0 {
            return 0.0;
        }
        cur * decay_weight(
            self.nanos(now)
                .saturating_sub(self.at.load(Ordering::Relaxed)),
        )
    }
}

impl fmt::Debug for PeakEwma {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeakEwma")
            .field(
                "seconds",
                &f64::from_bits(self.bits.load(Ordering::Relaxed)),
            )
            .field("at_nanos", &self.at.load(Ordering::Relaxed))
            .finish()
    }
}

/// e^(-elapsed / LATENCY_DECAY), as steer computes it.
fn decay_weight(elapsed_nanos: u64) -> f64 {
    (-(elapsed_nanos as f64) / LATENCY_DECAY.as_nanos() as f64).exp()
}

/// Power of two choices over a fixed set of endpoints.
pub struct LeastLoaded<E> {
    endpoints: Arc<[E]>,
    rng: Arc<dyn Rng>,
    clock: Arc<dyn Clock>,
}

impl<E> LeastLoaded<E> {
    /// A policy over `endpoints`, drawing from `rng` and reading costs as
    /// of `clock`'s time.
    pub fn new(endpoints: Arc<[E]>, rng: Arc<dyn Rng>, clock: Arc<dyn Clock>) -> LeastLoaded<E> {
        LeastLoaded {
            endpoints,
            rng,
            clock,
        }
    }

    /// The endpoints, indexed as picks are.
    pub fn endpoints(&self) -> &[E] {
        &self.endpoints
    }
}

impl<E> fmt::Debug for LeastLoaded<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LeastLoaded")
            .field("endpoints", &self.endpoints.len())
            .finish_non_exhaustive()
    }
}

impl<E: Cost + Send + Sync> Policy for LeastLoaded<E> {
    /// Picks among `cands`, never one in `tried`. With two or more left it
    /// draws two distinct ones and keeps the cheaper, the first drawn on a
    /// tie; with one left it takes it without drawing.
    fn pick(&self, cands: &[usize], tried: &[usize], _key: Option<u64>) -> Option<usize> {
        // steer copies the untried candidates into a new slice; counting
        // them and walking to the drawn positions picks the same ones
        // without allocating.
        let open = |i: &&usize| !tried.contains(i);
        let nth = |k: usize| -> usize {
            if tried.is_empty() {
                cands[k]
            } else {
                // k is below the number of open candidates.
                *cands.iter().filter(open).nth(k).unwrap_or(&cands[0])
            }
        };
        let len = if tried.is_empty() {
            cands.len()
        } else {
            cands.iter().filter(open).count()
        };
        match len {
            0 => return None,
            1 => return Some(nth(0)),
            _ => {}
        }
        let a = self.rng.below(len);
        let mut b = self.rng.below(len - 1);
        if b >= a {
            b += 1;
        }
        let (ia, ib) = (nth(a), nth(b));
        let now = self.clock.now();
        if self.endpoints[ib].cost(now) < self.endpoints[ia].cost(now) {
            Some(ib)
        } else {
            Some(ia)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicI64;

    use edgefront_core::clock::{ManualClock, MonotonicClock};

    use super::*;
    use crate::balancer::rng::SplitMix64;

    /// The parts of steer's endpoint state that the cost reads.
    #[derive(Debug)]
    struct Load {
        latency: PeakEwma,
        in_flight: AtomicI64,
    }

    impl Load {
        fn new(epoch: Instant) -> Load {
            Load {
                latency: PeakEwma::new(epoch),
                in_flight: AtomicI64::new(0),
            }
        }
    }

    impl Cost for Load {
        fn cost(&self, now: Instant) -> f64 {
            cost(
                self.latency.latency(now),
                self.in_flight.load(Ordering::Relaxed),
            )
        }
    }

    fn policy(n: usize, clock: &Arc<ManualClock>, seed: u64) -> LeastLoaded<Load> {
        let eps: Arc<[Load]> = (0..n).map(|_| Load::new(clock.start())).collect();
        LeastLoaded::new(eps, Arc::new(SplitMix64::new(seed)), clock.clone())
    }

    const MS: Duration = Duration::from_millis(1);

    // TestLeastLoadedPrefersCheaper.
    #[test]
    fn prefers_cheaper() {
        let clock = Arc::new(ManualClock::new());
        let ll = policy(3, &clock, 1);
        let now = clock.now();
        let e = ll.endpoints();
        e[0].latency.observe(100 * MS, now);
        e[1].latency.observe(MS, now);
        e[2].latency.observe(MS, now);
        e[2].in_flight.store(200, Ordering::Relaxed);
        let mut counts = [0u32; 3];
        for _ in 0..3000 {
            counts[ll.pick(&[0, 1, 2], &[], None).unwrap()] += 1;
        }
        // Costs: 0 = 0.1 x 1, 1 = 0.001 x 1, 2 = 0.001 x 201. Endpoint 1
        // wins every pair it is in (2/3 of picks) and 0 beats 2.
        assert!(counts[1] >= 1800 && counts[0] > 0, "{counts:?}");
        assert_eq!(counts[2], 0, "most expensive endpoint picked");
    }

    // TestLeastLoadedRetriesPenalisedEndpoint: a penalised endpoint that
    // gets no traffic is not starved forever.
    #[test]
    fn retries_penalised_endpoint() {
        let clock = Arc::new(ManualClock::new());
        let ll = policy(2, &clock, 2);
        let e = ll.endpoints();
        e[0].latency.observe(Duration::from_secs(1), clock.now()); // a failure penalty
        e[1].latency.observe(MS, clock.now());
        for _ in 0..100 {
            assert_eq!(ll.pick(&[0, 1], &[], None), Some(1));
        }
        e[1].latency.observe(MS, clock.now());
        // 1 s x e^-8 = 0.34 ms, below the fresh 1 ms of endpoint 1.
        clock.advance(Duration::from_secs(80));
        e[1].latency.observe(MS, clock.now());
        assert_eq!(ll.pick(&[0, 1], &[], None), Some(0));
    }

    // TestLeastLoadedSkipAndSmallSets.
    #[test]
    fn skip_and_small_sets() {
        let eps: Arc<[Load]> = (0..3).map(|_| Load::new(Instant::now())).collect();
        let ll = LeastLoaded::new(
            eps,
            Arc::new(SplitMix64::from_entropy()),
            MonotonicClock::shared(),
        );
        for _ in 0..50 {
            assert_eq!(ll.pick(&[0, 1, 2], &[0, 1], None), Some(2));
            assert_ne!(ll.pick(&[0, 1, 2], &[1], None), Some(1));
        }
        assert_eq!(ll.pick(&[0], &[], None), Some(0));
        assert_eq!(ll.pick(&[], &[], None), None);
        assert_eq!(ll.pick(&[0, 1], &[0, 1], None), None);
        assert_eq!(format!("{ll:?}"), "LeastLoaded { endpoints: 3, .. }");
    }

    #[test]
    fn draws_two_distinct_candidates_uniformly() {
        // Equal costs: the first drawn wins, so every open candidate is
        // picked about equally often, and never a tried one.
        let clock = Arc::new(ManualClock::new());
        let ll = policy(6, &clock, 3);
        let mut counts = [0u32; 6];
        for _ in 0..6000 {
            counts[ll.pick(&[0, 1, 2, 3, 4, 5], &[2, 4], None).unwrap()] += 1;
        }
        assert_eq!((counts[2], counts[4]), (0, 0));
        for i in [0, 1, 3, 5] {
            assert!((1300..1700).contains(&counts[i]), "{counts:?}");
        }
    }

    #[test]
    fn peak_ewma_follows_steer() {
        let start = Instant::now();
        let e = PeakEwma::new(start);
        assert_eq!(e.latency(start), 0.0);
        // The first sample sets the estimate, even a fast one.
        e.observe(Duration::from_millis(40), start);
        assert_eq!(e.latency(start), 0.04);
        // A slower sample replaces it at once.
        e.observe(Duration::from_millis(90), start);
        assert_eq!(e.latency(start), 0.09);
        // A faster one pulls it down by the decay weight.
        let later = start + LATENCY_DECAY;
        e.observe(Duration::from_millis(10), later);
        let w = (-1.0f64).exp();
        assert_eq!(e.latency(later), 0.09 * w + 0.01 * (1.0 - w));
        // Between samples it decays towards zero.
        let much_later = later + 3 * LATENCY_DECAY;
        let got = e.latency(much_later);
        assert!((got - e.latency(later) * (-3.0f64).exp()).abs() < 1e-15);
        // Time before the epoch or the last sample counts as no time.
        assert_eq!(e.latency(start), e.latency(later));
        assert!(format!("{e:?}").starts_with("PeakEwma { seconds: "));
    }

    #[test]
    fn cost_floors_latency_and_counts_in_flight() {
        assert_eq!(cost(0.0, 0), MIN_LATENCY);
        assert_eq!(cost(0.5, 3), 2.0);
        let shared: Arc<Load> = Arc::new(Load::new(Instant::now()));
        shared.in_flight.store(1, Ordering::Relaxed);
        assert_eq!(shared.cost(Instant::now()), 2.0 * MIN_LATENCY);
    }

    #[test]
    fn observe_survives_a_poisoned_lock() {
        let e = Arc::new(PeakEwma::new(Instant::now()));
        let e2 = Arc::clone(&e);
        let _ = std::thread::spawn(move || {
            let _guard = e2.observed.lock().unwrap();
            panic!("poison");
        })
        .join();
        e.observe(MS, Instant::now());
        assert!(e.latency(Instant::now()) > 0.0);
    }
}
