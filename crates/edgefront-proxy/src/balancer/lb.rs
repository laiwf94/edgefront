//! The load balancer of one cluster, ported from steer's `balancer.LB`: the
//! priority decision of [`priority`](super::priority) put together with a
//! [`Policy`] and the live [`Endpoint`] state.
//!
//! Every pick reads the endpoints' availability at the clock's time, counts
//! the healthy and total endpoints per priority level, and decides from
//! those counts where traffic goes:
//!
//! - in panic, the policy picks from every endpoint, available or not;
//! - when nothing is available and panic is off, there is no pick;
//! - otherwise a roll in `0..100` chooses a level by its load, the policy
//!   picks among that level's available endpoints, and if all of them were
//!   already tried by this request the other levels are tried in priority
//!   order.
//!
//! Maglev with a request key takes the roll from the key's high half, so a
//! key keeps its level, and so its endpoint, for as long as the health
//! picture holds; every other pick draws the roll from the shared
//! [`Rng`]. The draws happen in the same order as steer's, so a balancer
//! given the same seed, endpoints and clock makes the same picks as steer,
//! which is what `edgefront-difftest balancer` checks.

use std::fmt;
use std::sync::Arc;

use edgefront_core::chunked::Chunked;
use edgefront_core::clock::{Clock, MonotonicClock};
use edgefront_core::config::Policy as PolicyName;
use edgefront_core::config::limits::MAX_PRIORITY;

use super::Policy;
use super::ewma::LeastLoaded;
use super::maglev::{self, Maglev};
use super::priority::{Candidates, Census, Decision, LEVELS, PriorityState, hash_roll};
use super::rng::{Rng, SplitMix64};
use super::rr::{self, RoundRobin};
use crate::endpoint::Endpoint;

/// How to build a [`Balancer`], steer's `balancer.Options`.
#[derive(Debug, Clone)]
pub struct Options {
    /// The policy that picks within a level.
    pub policy: PolicyName,
    /// A percentage; zero disables panic routing.
    pub panic_threshold: u8,
    /// The time availability and latency are read at.
    pub clock: Arc<dyn Clock>,
    /// The source of every random draw: level rolls and policy draws.
    pub rng: Arc<dyn Rng>,
    /// Maglev's table size, a prime larger than the endpoint count.
    pub maglev_table_size: usize,
}

impl Options {
    /// Options for `policy` with steer's defaults: no panic threshold, the
    /// monotonic clock, a generator seeded from entropy and Maglev's
    /// default table size.
    pub fn new(policy: PolicyName) -> Options {
        Options {
            policy,
            panic_threshold: 0,
            clock: MonotonicClock::shared(),
            rng: Arc::new(SplitMix64::from_entropy()),
            maglev_table_size: maglev::DEFAULT_TABLE_SIZE,
        }
    }

    /// The same options with this panic threshold.
    pub fn panic_threshold(mut self, percent: u8) -> Options {
        self.panic_threshold = percent;
        self
    }

    /// The same options reading this clock.
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Options {
        self.clock = clock;
        self
    }

    /// The same options drawing from this generator.
    pub fn rng(mut self, rng: Arc<dyn Rng>) -> Options {
        self.rng = rng;
        self
    }

    /// The same options with this Maglev table size.
    pub fn maglev_table_size(mut self, size: usize) -> Options {
        self.maglev_table_size = size;
        self
    }
}

/// Why a [`Balancer`] could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A cluster needs at least one endpoint.
    NoEndpoints,
    /// An endpoint's weight is zero or its priority is above
    /// [`MAX_PRIORITY`].
    Endpoint {
        address: Box<str>,
        weight: u32,
        priority: u32,
    },
    /// The Maglev table size does not fit the endpoints.
    Maglev(maglev::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoEndpoints => f.write_str("balancer: no endpoints"),
            Error::Endpoint {
                address,
                weight,
                priority,
            } => write!(
                f,
                "balancer: endpoint {address} has weight {weight} priority {priority}"
            ),
            Error::Maglev(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

/// Balances one cluster's requests over a fixed set of endpoints. It is
/// safe to share between threads.
///
/// The endpoints are a [`Chunked`] list, and [`Balancer::patch`] makes the
/// balancer for a few endpoints more or fewer at the cost of those few: a
/// discovery change to a cluster of 100,000 endpoints does not rebuild
/// it. Requests that started on the old balancer finish on it.
pub struct Balancer {
    endpoints: Chunked<Endpoint>,
    /// How many endpoints each priority level has, so a patch knows the
    /// levels without a scan.
    counts: [u32; LEVELS],
    /// The highest priority plus one.
    levels: usize,
    picker: Picker,
    opts: Options,
}

/// The policy of a [`Balancer`].
enum Picker {
    RoundRobin(RoundRobin),
    LeastLoaded(LeastLoaded<Endpoint>),
    Maglev(Maglev),
}

impl Picker {
    fn policy(&self) -> &dyn Policy {
        match self {
            Picker::RoundRobin(p) => p,
            Picker::LeastLoaded(p) => p,
            Picker::Maglev(p) => p,
        }
    }
}

impl fmt::Debug for Balancer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Balancer")
            .field("endpoints", &self.endpoints.len())
            .field("levels", &self.levels)
            .field("hashing", &self.hashing())
            .field("panic_threshold", &self.opts.panic_threshold)
            .finish_non_exhaustive()
    }
}

/// The checked priority of `e`, as a level index.
fn level_of(e: &Endpoint) -> Result<usize, Error> {
    match u8::try_from(e.priority) {
        Ok(p) if e.weight >= 1 && p <= MAX_PRIORITY => Ok(usize::from(p)),
        _ => Err(Error::Endpoint {
            address: e.address.as_ref().into(),
            weight: e.weight,
            priority: e.priority,
        }),
    }
}

/// The level of an endpoint a balancer holds, whose priority was checked
/// when it joined.
fn priority(e: &Endpoint) -> u8 {
    // At most MAX_PRIORITY, checked by level_of.
    e.priority as u8
}

/// A [`Balancer`] being changed by a few endpoints: from
/// [`Balancer::patch`], or from nothing with [`Patch::new`].
///
/// Removal is by swap, as [`Vec::swap_remove`]: the last endpoint takes
/// the removed one's index, so a caller that keeps an index by address
/// moves only that one. [`Patch::finish`] gives the new balancer; the one
/// patched is never changed.
pub struct Patch<'a> {
    base: Option<&'a Balancer>,
    endpoints: Chunked<Endpoint>,
    counts: [u32; LEVELS],
    /// What a round robin carried over from `base` has to replay.
    changes: Vec<rr::Change>,
    /// The first endpoint refused, which fails the patch.
    error: Option<Error>,
    opts: Options,
}

impl Patch<'static> {
    /// A patch from no endpoints.
    pub fn new(opts: Options) -> Patch<'static> {
        Patch {
            base: None,
            endpoints: Chunked::new(),
            counts: [0; LEVELS],
            changes: Vec::new(),
            error: None,
            opts,
        }
    }
}

impl Patch<'_> {
    /// The endpoints as the patch has them so far.
    pub fn endpoints(&self) -> &Chunked<Endpoint> {
        &self.endpoints
    }

    /// How many endpoints the patch has so far.
    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    /// Whether the patch has no endpoints so far.
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    /// Counts `e` in its level, or keeps the error that refuses it.
    fn count(&mut self, e: &Endpoint, add: bool) {
        match level_of(e) {
            Ok(p) if add => self.counts[p] += 1,
            Ok(p) => self.counts[p] -= 1,
            Err(err) => {
                self.error.get_or_insert(err);
            }
        }
    }

    /// Adds `e` at the end, at index `len()` before the call.
    pub fn push(&mut self, e: Endpoint) {
        self.count(&e, true);
        self.changes.push(rr::Change::Push(e.weight));
        self.endpoints.push(e);
    }

    /// Puts `e` at `index` in place of the endpoint there, which it
    /// returns: for an endpoint whose weight or priority changed.
    ///
    /// # Panics
    ///
    /// If `index` is out of bounds.
    pub fn set(&mut self, index: usize, e: Endpoint) -> Endpoint {
        self.count(&e, true);
        self.changes.push(rr::Change::Reweight(index, e.weight));
        let old = self.endpoints.set(index, e);
        self.count(&old, false);
        old
    }

    /// Removes the endpoint at `index` and returns it; the last endpoint
    /// takes its index.
    ///
    /// # Panics
    ///
    /// If `index` is out of bounds.
    pub fn swap_remove(&mut self, index: usize) -> Endpoint {
        let old = self.endpoints.swap_remove(index);
        self.changes.push(rr::Change::SwapRemove(index));
        self.count(&old, false);
        old
    }

    /// The balancer over the patched endpoints. A round robin carries its
    /// rotation over from the balancer patched; Maglev builds its table
    /// again, since its lookup table depends on every endpoint (it is
    /// bounded by the table size, 65,537 slots by default).
    pub fn finish(self) -> Result<Balancer, Error> {
        if let Some(e) = self.error {
            return Err(e);
        }
        if self.endpoints.is_empty() {
            return Err(Error::NoEndpoints);
        }
        let levels = self.counts.iter().rposition(|&n| n > 0).unwrap_or(0) + 1;
        let opts = self.opts;
        let picker = match (opts.policy, self.base.map(|b| &b.picker)) {
            (PolicyName::RoundRobin, Some(Picker::RoundRobin(old))) => {
                Picker::RoundRobin(old.patched(&self.changes))
            }
            (PolicyName::RoundRobin, _) => {
                let weights: Vec<u32> = self.endpoints.iter().map(|e| e.weight).collect();
                Picker::RoundRobin(RoundRobin::new(&weights))
            }
            (PolicyName::LeastLoaded, _) => Picker::LeastLoaded(LeastLoaded::new(
                self.endpoints.clone(),
                Arc::clone(&opts.rng),
                Arc::clone(&opts.clock),
            )),
            (PolicyName::Maglev, _) => {
                let eps: Vec<maglev::Endpoint> = self
                    .endpoints
                    .iter()
                    .map(|e| maglev::Endpoint::new(&*e.address, e.weight))
                    .collect();
                let m = Maglev::new(&eps, opts.maglev_table_size, Arc::clone(&opts.rng))
                    .map_err(Error::Maglev)?;
                Picker::Maglev(m)
            }
        };
        Ok(Balancer {
            endpoints: self.endpoints,
            counts: self.counts,
            levels,
            picker,
            opts,
        })
    }
}

impl Balancer {
    /// A balancer over `endpoints`, steer's `balancer.New`. Every endpoint
    /// needs a weight of at least one and a priority of at most
    /// [`MAX_PRIORITY`].
    pub fn new(
        endpoints: impl IntoIterator<Item = Endpoint>,
        opts: Options,
    ) -> Result<Balancer, Error> {
        let mut patch = Patch::new(opts);
        for e in endpoints {
            patch.push(e);
        }
        patch.finish()
    }

    /// A patch from this balancer's endpoints, for [`Patch::finish`] to
    /// make the next balancer from. The patch shares the endpoint list, so
    /// starting one costs O(1).
    pub fn patch(&self) -> Patch<'_> {
        Patch {
            base: Some(self),
            endpoints: self.endpoints.clone(),
            counts: self.counts,
            changes: Vec::new(),
            error: None,
            opts: self.opts.clone(),
        }
    }

    /// Whether the policy hashes request keys (Maglev).
    fn hashing(&self) -> bool {
        matches!(self.picker, Picker::Maglev(_))
    }

    /// The endpoints, indexed as picks are: in configuration order for a
    /// new balancer, and for a patched one in the order its patches left
    /// (a removal moves the last endpoint into the hole).
    pub fn endpoints(&self) -> &Chunked<Endpoint> {
        &self.endpoints
    }

    /// How many priority levels the cluster has: its highest priority plus
    /// one.
    pub fn levels(&self) -> usize {
        self.levels
    }

    /// The healthy and total endpoint counts per level as of now.
    pub fn census(&self) -> Census {
        let now = self.opts.clock.now();
        let mut census = Census::new(self.levels);
        for e in &self.endpoints {
            census.add(priority(e), e.state.available(now));
        }
        census
    }

    /// The priority decision as of now.
    pub fn decision(&self) -> Decision {
        self.census().decide(self.opts.panic_threshold)
    }

    /// The endpoint for the next attempt of a request, never one in
    /// `tried`, or `None` when no available endpoint is left. `key` is the
    /// request's hash key, used by Maglev.
    pub fn pick(&self, key: Option<u64>, tried: &[usize]) -> Option<usize> {
        let now = self.opts.clock.now();
        // The available endpoints grouped by level, in configuration order
        // within each level, as steer's per-level lists are.
        let mut census = Census::new(self.levels);
        let mut avail = vec![0usize; self.endpoints.len()];
        let mut ends = [0usize; LEVELS];
        let mut available = Vec::with_capacity(self.endpoints.len());
        for e in &self.endpoints {
            let a = e.state.available(now);
            census.add(priority(e), a);
            available.push(a);
        }
        let mut start = 0;
        for (p, end) in ends.iter_mut().enumerate().take(self.levels) {
            start += census.healthy()[p] as usize;
            *end = start;
        }
        let mut next = [0usize; LEVELS];
        next[1..self.levels].copy_from_slice(&ends[..self.levels - 1]);
        for (i, (e, &a)) in self.endpoints.iter().zip(&available).enumerate() {
            if a {
                let slot = &mut next[usize::from(priority(e))];
                avail[*slot] = i;
                *slot += 1;
            }
        }
        let level = |p: usize| {
            let from = if p == 0 { 0 } else { ends[p - 1] };
            &avail[from..ends[p]]
        };
        let decision = census.decide(self.opts.panic_threshold);
        let roll = match (decision, key) {
            (Decision::Spread(_), Some(k)) if self.hashing() => hash_roll(k),
            // Below 100, so the narrowing never loses anything.
            (Decision::Spread(_), _) => self.opts.rng.below(100) as u8,
            // No level is chosen, so steer draws nothing.
            (Decision::Panic | Decision::Unavailable, _) => 0,
        };
        let policy = self.picker.policy();
        decision.route(roll, |c| match c {
            Candidates::All => {
                let all: Vec<usize> = (0..self.endpoints.len()).collect();
                policy.pick(&all, tried, key)
            }
            Candidates::Level(p) => policy.pick(level(p), tried, key),
        })
    }

    /// The priority picture as of now, steer's `LB.State`.
    pub fn state(&self) -> PriorityState {
        self.census().state(self.opts.panic_threshold)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use edgefront_core::clock::ManualClock;

    use super::*;

    /// steer's `eps`: endpoints of these `(weight, priority)` at
    /// 10.0.0.N:80.
    fn eps(specs: &[(u32, u32)], epoch: Instant) -> Vec<Endpoint> {
        specs
            .iter()
            .enumerate()
            .map(|(i, &(w, p))| Endpoint::new(&format!("10.0.0.{}:80", i + 1), w, p, epoch))
            .collect()
    }

    fn lb(e: &[Endpoint], policy: PolicyName, panic: u8, clock: &Arc<ManualClock>) -> Balancer {
        let opts = Options::new(policy)
            .panic_threshold(panic)
            .clock(Arc::clone(clock) as Arc<dyn Clock>)
            .rng(Arc::new(SplitMix64::new(1)));
        Balancer::new(e.to_vec(), opts).unwrap()
    }

    fn share(lb: &Balancer, n: usize) -> Vec<usize> {
        let mut counts = vec![0; lb.endpoints().len()];
        for _ in 0..10_000 {
            counts[lb.pick(None, &[]).expect("a pick")] += 1;
        }
        assert_eq!(counts.len(), n);
        counts
    }

    // TestLBPriorityFailover
    #[test]
    fn priority_failover() {
        let clock = Arc::new(ManualClock::new());
        let e = eps(&[(1, 0), (1, 0), (1, 1)], clock.start());
        let lb = lb(&e, PolicyName::RoundRobin, 50, &clock);
        assert_eq!(share(&lb, 3)[2], 0, "all healthy, the backup got traffic");
        e[0].state.set_healthy(false); // P0 50% healthy: health 70, a 70/30 split
        let c = share(&lb, 3);
        assert_eq!(c[0], 0, "{c:?}");
        assert!((c[2] as f64 / 10_000.0 - 0.30).abs() <= 0.03, "{c:?}");
        let st = lb.state();
        assert_eq!(st.priority_load, [70, 30]);
        assert!(!st.panic);
        e[1].state.eject(
            clock.now(),
            Duration::from_secs(60),
            Duration::from_secs(3600),
            "test",
        ); // P0 empty: everything to P1
        assert_eq!(share(&lb, 3)[2], 10_000);
        clock.advance(Duration::from_secs(120)); // the ejection is over
        assert!(share(&lb, 3)[1] > 0, "ejection expiry ignored");
    }

    // TestLBPanicRoutesToEveryone
    #[test]
    fn panic_routes_to_everyone() {
        let clock = Arc::new(ManualClock::new());
        let e = eps(&[(1, 0); 5], clock.start());
        let balancer = lb(&e, PolicyName::RoundRobin, 50, &clock);
        for ep in &e[..4] {
            ep.state.set_healthy(false); // 20% healthy: health 28 < 50
        }
        let mut counts = [0; 5];
        for _ in 0..500 {
            counts[balancer.pick(None, &[]).unwrap()] += 1;
        }
        assert_eq!(counts, [100; 5], "panic should spread evenly");
        assert!(balancer.state().panic);
        assert_eq!(balancer.decision(), Decision::Panic);
        // Without a panic threshold the last healthy host takes everything.
        let balancer = lb(&e, PolicyName::RoundRobin, 0, &clock);
        for _ in 0..50 {
            assert_eq!(balancer.pick(None, &[]), Some(4));
        }
        e[4].state.set_healthy(false);
        assert_eq!(balancer.pick(None, &[]), None);
        assert_eq!(balancer.decision(), Decision::Unavailable);
        let st = balancer.state();
        assert_eq!(st.priority_load, [0]);
        assert!(!st.panic);
    }

    // TestLBNeverRepeatsAndFallsBack
    #[test]
    fn never_repeats_and_falls_back() {
        for policy in [
            PolicyName::RoundRobin,
            PolicyName::LeastLoaded,
            PolicyName::Maglev,
        ] {
            let clock = Arc::new(ManualClock::new());
            let e = eps(&[(1, 0), (1, 0), (1, 1)], clock.start());
            let lb = lb(&e, policy, 50, &clock);
            for iter in 0..200u64 {
                let key = iter.wrapping_mul(0x9e37_79b9_7f4a_7c15);
                let mut tried = Vec::new();
                for _ in 0..3 {
                    let i = lb.pick(Some(key), &tried);
                    let i = i.unwrap_or_else(|| panic!("{policy:?}: no pick after {tried:?}"));
                    assert!(!tried.contains(&i), "{policy:?}: {i} after {tried:?}");
                    tried.push(i);
                }
                assert_eq!(lb.pick(Some(key), &tried), None, "{policy:?}");
            }
        }
    }

    // TestLBHashKeepsKeyOnEndpoint
    #[test]
    fn hash_keeps_key_on_endpoint() {
        let clock = Arc::new(ManualClock::new());
        let e = eps(&[(1, 0), (1, 0), (1, 0), (1, 1)], clock.start());
        let lb = lb(&e, PolicyName::Maglev, 50, &clock);
        // P0 health 93: keys split between the levels by key, not by chance.
        e[0].state.set_healthy(false);
        let mut levels = [0; 2];
        for k in 0..500 {
            let key = maglev::hash_key(format!("session-{k}").as_bytes());
            let first = lb.pick(Some(key), &[]).unwrap();
            levels[e[first].priority as usize] += 1;
            for _ in 0..5 {
                assert_eq!(lb.pick(Some(key), &[]), Some(first), "key {k} moved");
            }
        }
        assert!(levels[0] > 0 && levels[1] > 0, "{levels:?}");
    }

    // TestNewValidates
    #[test]
    fn new_validates() {
        let now = Instant::now();
        let opts = || Options::new(PolicyName::RoundRobin);
        assert_eq!(
            Balancer::new(Vec::new(), opts()).unwrap_err(),
            Error::NoEndpoints
        );
        let err = Balancer::new(eps(&[(0, 0)], now), opts()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "balancer: endpoint 10.0.0.1:80 has weight 0 priority 0"
        );
        let err = Balancer::new(eps(&[(1, 8)], now), opts()).unwrap_err();
        assert!(matches!(err, Error::Endpoint { priority: 8, .. }), "{err}");
        let err = Balancer::new(eps(&[(1, u32::MAX)], now), opts()).unwrap_err();
        assert!(matches!(err, Error::Endpoint { .. }), "{err}");
        let err = Balancer::new(
            eps(&[(1, 0), (1, 0)], now),
            Options::new(PolicyName::Maglev).maglev_table_size(2),
        )
        .unwrap_err();
        assert_eq!(
            err,
            Error::Maglev(maglev::Error::TableSize {
                size: 2,
                endpoints: 2
            })
        );
        assert_eq!(
            err.to_string(),
            maglev::Error::TableSize {
                size: 2,
                endpoints: 2
            }
            .to_string()
        );
        let lb = Balancer::new(eps(&[(1, 0)], now), opts()).unwrap();
        assert_eq!(lb.endpoints().len(), 1);
        assert_eq!(lb.levels(), 1);
        assert!(format!("{lb:?}").starts_with("Balancer { endpoints: 1, levels: 1"));
        // A gap in the priorities still counts as a level.
        let lb = Balancer::new(eps(&[(1, 0), (1, 2)], now), opts()).unwrap();
        assert_eq!(lb.levels(), 3);
        assert_eq!(lb.state().total, [1, 0, 1]);
    }

    /// A seeded balancer of `policy` over `e` with a small Maglev table.
    fn seeded(e: Vec<Endpoint>, policy: PolicyName, clock: &Arc<ManualClock>) -> Balancer {
        Balancer::new(e, seeded_opts(policy, clock)).unwrap()
    }

    fn seeded_opts(policy: PolicyName, clock: &Arc<ManualClock>) -> Options {
        Options::new(policy)
            .panic_threshold(50)
            .clock(Arc::clone(clock) as Arc<dyn Clock>)
            .rng(Arc::new(SplitMix64::new(5)))
            .maglev_table_size(1031)
    }

    fn addresses(b: &Balancer) -> Vec<(String, u32, u32)> {
        b.endpoints()
            .iter()
            .map(|e| (e.address.to_string(), e.weight, e.priority))
            .collect()
    }

    /// Patches of every kind, over several chunks, give the balancer that
    /// a balancer built from the same endpoints in the same order is: the
    /// same endpoints, levels and priority picture, and for the policies
    /// that keep no rotation the same picks from the same seed. The
    /// balancer patched is left as it was.
    #[test]
    fn a_patch_matches_a_balancer_built_from_its_result() {
        let clock = Arc::new(ManualClock::new());
        let epoch = clock.start();
        let ep = |i: u32, w: u32, p: u32| {
            Endpoint::new(&format!("10.1.{}.{}:80", i / 200, i % 200), w, p, epoch)
        };
        for policy in [
            PolicyName::RoundRobin,
            PolicyName::LeastLoaded,
            PolicyName::Maglev,
        ] {
            let mut model: Vec<Endpoint> = (0..700).map(|i| ep(i, 1 + i % 3, 0)).collect();
            let mut b = seeded(model.clone(), policy, &clock);
            let mut next = 700;
            let rng = SplitMix64::new(11);
            for round in 0..30 {
                let before = addresses(&b);
                // A fresh generator of the same seed as `built` below.
                let mut patch = Patch {
                    opts: seeded_opts(policy, &clock),
                    ..b.patch()
                };
                for _ in 0..1 + rng.below(40) {
                    match rng.below(3) {
                        0 if model.len() > 1 => {
                            let i = rng.below(model.len());
                            let want = model.swap_remove(i);
                            assert_eq!(patch.swap_remove(i).address, want.address);
                        }
                        1 => {
                            let i = rng.below(model.len());
                            let e = Endpoint {
                                weight: 1 + rng.below(5) as u32,
                                priority: rng.below(3) as u32,
                                ..model[i].clone()
                            };
                            model[i] = e.clone();
                            patch.set(i, e);
                        }
                        _ => {
                            let e = ep(next, 1, rng.below(2) as u32);
                            next += 1;
                            model.push(e.clone());
                            patch.push(e);
                        }
                    }
                }
                assert_eq!(patch.len(), model.len());
                let patched = patch.finish().unwrap();
                assert_eq!(addresses(&b), before, "round {round}: the original changed");
                let built = seeded(model.clone(), policy, &clock);
                assert_eq!(addresses(&patched), addresses(&built), "round {round}");
                assert_eq!(patched.levels(), built.levels(), "round {round}");
                assert_eq!(patched.state(), built.state(), "round {round}");
                if policy != PolicyName::RoundRobin {
                    // Same seed, same endpoints: same picks.
                    for k in 0..50u64 {
                        let key = Some(k.wrapping_mul(0x9e37_79b9_7f4a_7c15));
                        let want = built.pick(key, &[]);
                        assert_eq!(patched.pick(key, &[]), want, "{policy:?} round {round}");
                    }
                }
                b = patched;
            }
        }
    }

    /// A round robin keeps its rotation across a patch, so endpoints that
    /// stay go on taking turns instead of the first ones taking every
    /// request after each change.
    #[test]
    fn a_round_robin_keeps_its_rotation_across_patches() {
        let clock = Arc::new(ManualClock::new());
        let e = eps(&[(1, 0); 10], clock.start());
        let mut b = seeded(e, PolicyName::RoundRobin, &clock);
        let mut counts = std::collections::HashMap::new();
        // One pick between patches, as at 1,000 changes a second and as
        // many requests: a rebuilt round robin would pick 0 every time.
        for i in 0..100 {
            let pick = b.pick(None, &[]).unwrap();
            *counts
                .entry(b.endpoints()[pick].address.to_string())
                .or_insert(0) += 1;
            let mut patch = b.patch();
            patch.push(Endpoint::new(
                &format!("10.9.0.{i}:80"),
                1,
                1,
                clock.start(),
            ));
            b = patch.finish().unwrap();
        }
        assert_eq!(counts.len(), 10, "{counts:?}");
        assert!(counts.values().all(|&n| n == 10), "{counts:?}");
    }

    #[test]
    fn a_patch_refuses_what_a_new_balancer_refuses() {
        let clock = Arc::new(ManualClock::new());
        let now = clock.start();
        let b = seeded(eps(&[(1, 0), (1, 1)], now), PolicyName::RoundRobin, &clock);
        let mut patch = b.patch();
        patch.push(Endpoint::new("10.0.0.9:80", 0, 0, now));
        assert!(matches!(
            patch.finish().unwrap_err(),
            Error::Endpoint { weight: 0, .. }
        ));
        let mut patch = b.patch();
        patch.set(0, Endpoint::new("10.0.0.1:80", 1, 8, now));
        assert!(matches!(
            patch.finish().unwrap_err(),
            Error::Endpoint { priority: 8, .. }
        ));
        // Emptied: no balancer.
        let mut patch = b.patch();
        patch.swap_remove(1);
        patch.swap_remove(0);
        assert!(patch.is_empty());
        assert_eq!(patch.finish().unwrap_err(), Error::NoEndpoints);
        // The highest level going takes its level with it.
        let mut patch = b.patch();
        patch.swap_remove(1);
        let one = patch.finish().unwrap();
        assert_eq!(one.levels(), 1);
        // From nothing, as a cluster that discovery emptied.
        let mut patch = Patch::new(seeded_opts(PolicyName::Maglev, &clock));
        assert_eq!(patch.endpoints().len(), 0);
        patch.push(Endpoint::new("10.0.0.1:80", 1, 2, now));
        let b = patch.finish().unwrap();
        assert_eq!(b.levels(), 3);
        assert_eq!(b.pick(Some(7), &[]), Some(0));
        // Maglev still refuses more endpoints than its table has slots.
        let mut patch = Patch::new(seeded_opts(PolicyName::Maglev, &clock).maglev_table_size(2));
        patch.push(Endpoint::new("10.0.0.1:80", 1, 0, now));
        patch.push(Endpoint::new("10.0.0.2:80", 1, 0, now));
        assert!(matches!(patch.finish().unwrap_err(), Error::Maglev(_)));
    }

    #[test]
    fn levels_are_grouped_in_configuration_order() {
        // Endpoints listed out of priority order: each level's candidates
        // are its available endpoints in configuration order, so a round
        // robin over level 1 alternates 0 and 3 and never sees 1 or 2.
        let clock = Arc::new(ManualClock::new());
        let e = eps(&[(1, 1), (1, 0), (1, 0), (1, 1)], clock.start());
        let lb = lb(&e, PolicyName::RoundRobin, 0, &clock);
        e[1].state.set_healthy(false);
        e[2].state.set_healthy(false);
        let picks: Vec<_> = (0..6).map(|_| lb.pick(None, &[]).unwrap()).collect();
        assert_eq!(picks, [0, 3, 0, 3, 0, 3]);
        // A retry falls back to level 0 once level 1 is used up, but level 0
        // has nothing available.
        assert_eq!(lb.pick(None, &[0, 3]), None);
        e[2].state.set_healthy(true);
        assert_eq!(lb.pick(None, &[0, 3]), Some(2));
    }

    #[test]
    fn steer_draws_the_roll_only_when_a_level_is_chosen() {
        // In panic and with nothing available no roll is drawn, so a
        // keyless Maglev pick draws its start from the generator's first
        // output, as steer's does.
        let clock = Arc::new(ManualClock::new());
        let e = eps(&[(1, 0); 4], clock.start());
        for ep in &e[..3] {
            ep.state.set_healthy(false);
        }
        let first = SplitMix64::new(9).below(4);
        let opts = Options::new(PolicyName::Maglev)
            .panic_threshold(50)
            .clock(Arc::clone(&clock) as Arc<dyn Clock>)
            .rng(Arc::new(SplitMix64::new(9)))
            .maglev_table_size(13);
        let lb = Balancer::new(e.clone(), opts).unwrap();
        assert_eq!(lb.pick(None, &[]), Some(first));
        // Spread: the roll comes first, then the policy's own draw.
        let g = SplitMix64::new(9);
        let _roll = g.below(100);
        let start = g.below(4);
        e[0].state.set_healthy(true);
        e[1].state.set_healthy(true);
        e[2].state.set_healthy(true);
        let opts = Options::new(PolicyName::Maglev)
            .clock(Arc::clone(&clock) as Arc<dyn Clock>)
            .rng(Arc::new(SplitMix64::new(9)))
            .maglev_table_size(13);
        let lb = Balancer::new(e, opts).unwrap();
        assert_eq!(lb.pick(None, &[]), Some(start));
    }
}
