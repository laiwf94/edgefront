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

use edgefront_core::clock::{Clock, MonotonicClock};
use edgefront_core::config::Policy as PolicyName;
use edgefront_core::config::limits::MAX_PRIORITY;

use super::Policy;
use super::ewma::LeastLoaded;
use super::maglev::{self, Maglev};
use super::priority::{Candidates, Census, Decision, LEVELS, PriorityState, hash_roll};
use super::rng::{Rng, SplitMix64};
use super::rr::RoundRobin;
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
pub struct Balancer {
    endpoints: Arc<[Endpoint]>,
    /// Every endpoint index, the candidates in panic.
    all: Box<[usize]>,
    /// Each endpoint's priority, read on every pick.
    priorities: Box<[u8]>,
    /// The highest priority plus one.
    levels: usize,
    policy: Box<dyn Policy>,
    /// Whether the policy hashes request keys (Maglev).
    hashing: bool,
    panic_threshold: u8,
    clock: Arc<dyn Clock>,
    rng: Arc<dyn Rng>,
}

impl fmt::Debug for Balancer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Balancer")
            .field("endpoints", &self.endpoints.len())
            .field("levels", &self.levels)
            .field("hashing", &self.hashing)
            .field("panic_threshold", &self.panic_threshold)
            .finish_non_exhaustive()
    }
}

impl Balancer {
    /// A balancer over `endpoints`, steer's `balancer.New`. Every endpoint
    /// needs a weight of at least one and a priority of at most
    /// [`MAX_PRIORITY`].
    pub fn new(endpoints: impl Into<Arc<[Endpoint]>>, opts: Options) -> Result<Balancer, Error> {
        let endpoints: Arc<[Endpoint]> = endpoints.into();
        if endpoints.is_empty() {
            return Err(Error::NoEndpoints);
        }
        let mut priorities = Vec::with_capacity(endpoints.len());
        for e in endpoints.iter() {
            match u8::try_from(e.priority) {
                Ok(p) if e.weight >= 1 && p <= MAX_PRIORITY => priorities.push(p),
                _ => {
                    return Err(Error::Endpoint {
                        address: e.address.clone(),
                        weight: e.weight,
                        priority: e.priority,
                    });
                }
            }
        }
        let levels = usize::from(priorities.iter().copied().max().unwrap_or(0)) + 1;
        let (policy, hashing): (Box<dyn Policy>, bool) = match opts.policy {
            PolicyName::RoundRobin => {
                let weights: Vec<u32> = endpoints.iter().map(|e| e.weight).collect();
                (Box::new(RoundRobin::new(&weights)), false)
            }
            PolicyName::LeastLoaded => (
                Box::new(LeastLoaded::new(
                    Arc::clone(&endpoints),
                    Arc::clone(&opts.rng),
                    Arc::clone(&opts.clock),
                )),
                false,
            ),
            PolicyName::Maglev => {
                let eps: Vec<maglev::Endpoint> = endpoints
                    .iter()
                    .map(|e| maglev::Endpoint::new(&*e.address, e.weight))
                    .collect();
                let m = Maglev::new(&eps, opts.maglev_table_size, Arc::clone(&opts.rng))
                    .map_err(Error::Maglev)?;
                (Box::new(m), true)
            }
        };
        Ok(Balancer {
            all: (0..endpoints.len()).collect(),
            priorities: priorities.into(),
            levels,
            endpoints,
            policy,
            hashing,
            panic_threshold: opts.panic_threshold,
            clock: opts.clock,
            rng: opts.rng,
        })
    }

    /// The endpoints in configuration order, indexed as picks are.
    pub fn endpoints(&self) -> &[Endpoint] {
        &self.endpoints
    }

    /// How many priority levels the cluster has: its highest priority plus
    /// one.
    pub fn levels(&self) -> usize {
        self.levels
    }

    /// The healthy and total endpoint counts per level as of now.
    pub fn census(&self) -> Census {
        let now = self.clock.now();
        let mut census = Census::new(self.levels);
        for (e, &p) in self.endpoints.iter().zip(self.priorities.iter()) {
            census.add(p, e.state.available(now));
        }
        census
    }

    /// The priority decision as of now.
    pub fn decision(&self) -> Decision {
        self.census().decide(self.panic_threshold)
    }

    /// The endpoint for the next attempt of a request, never one in
    /// `tried`, or `None` when no available endpoint is left. `key` is the
    /// request's hash key, used by Maglev.
    pub fn pick(&self, key: Option<u64>, tried: &[usize]) -> Option<usize> {
        let now = self.clock.now();
        // The available endpoints grouped by level, in configuration order
        // within each level, as steer's per-level lists are.
        let mut census = Census::new(self.levels);
        let mut avail = vec![0usize; self.endpoints.len()];
        let mut ends = [0usize; LEVELS];
        let mut available = Vec::with_capacity(self.endpoints.len());
        for (e, &p) in self.endpoints.iter().zip(self.priorities.iter()) {
            let a = e.state.available(now);
            census.add(p, a);
            available.push(a);
        }
        let mut start = 0;
        for (p, end) in ends.iter_mut().enumerate().take(self.levels) {
            start += census.healthy()[p] as usize;
            *end = start;
        }
        let mut next = [0usize; LEVELS];
        next[1..self.levels].copy_from_slice(&ends[..self.levels - 1]);
        for (i, (&p, &a)) in self.priorities.iter().zip(&available).enumerate() {
            if a {
                let slot = &mut next[usize::from(p)];
                avail[*slot] = i;
                *slot += 1;
            }
        }
        let level = |p: usize| {
            let from = if p == 0 { 0 } else { ends[p - 1] };
            &avail[from..ends[p]]
        };
        let decision = census.decide(self.panic_threshold);
        let roll = match (decision, key) {
            (Decision::Spread(_), Some(k)) if self.hashing => hash_roll(k),
            // Below 100, so the narrowing never loses anything.
            (Decision::Spread(_), _) => self.rng.below(100) as u8,
            // No level is chosen, so steer draws nothing.
            (Decision::Panic | Decision::Unavailable, _) => 0,
        };
        decision.route(roll, |c| match c {
            Candidates::All => self.policy.pick(&self.all, tried, key),
            Candidates::Level(p) => self.policy.pick(level(p), tried, key),
        })
    }

    /// The priority picture as of now, steer's `LB.State`.
    pub fn state(&self) -> PriorityState {
        self.census().state(self.panic_threshold)
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
