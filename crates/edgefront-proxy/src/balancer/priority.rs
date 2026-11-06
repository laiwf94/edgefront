//! Priority levels: how much of a cluster's traffic each level takes, ported
//! from the priority half of steer's `internal/balancer`.
//!
//! Every endpoint has a priority, 0 (the primaries) to
//! [`MAX_PRIORITY`](edgefront_core::config::limits::MAX_PRIORITY). Traffic
//! goes to the lowest level while enough of it is healthy and spills to the
//! next levels as it loses hosts, following Envoy's priority load math:
//!
//! ```text
//! health[p]  = min(100, 1.4 x 100 x healthy[p] / total[p])
//! normalized = min(100, sum(health))
//! load[p]    = min(remaining, health[p] x 100 / normalized), in priority order
//! ```
//!
//! all in integers, with the rounding leftovers going to the first level
//! that has any health. The overprovisioning factor 1.4 means a level that
//! is at least 1/1.4 (about 72%) healthy still takes all of its load. When
//! the normalized health falls below the panic threshold, health is ignored
//! and the request may go to any endpoint of the cluster: a cluster that
//! has lost most of its hosts spreads load over all of them rather than
//! crushing the few that are left.
//!
//! The decisions are steer's, value for value. The only change is in the
//! data: loads and per-level counts live in fixed arrays of
//! [`LEVELS`] entries, so computing them on every pick allocates nothing.

use edgefront_core::config::limits::MAX_PRIORITY;

/// The most priority levels a cluster can have.
pub const LEVELS: usize = MAX_PRIORITY as usize + 1;

/// Envoy's default overprovisioning factor 1.4, in percent.
pub const OVERPROVISIONING_FACTOR: u64 = 140;

/// The percentage of traffic each priority level takes. The percentages
/// sum to 100.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loads {
    levels: u8,
    percent: [u8; LEVELS],
}

impl Loads {
    /// One percentage per level, in priority order.
    pub fn as_slice(&self) -> &[u8] {
        &self.percent[..usize::from(self.levels)]
    }

    /// How many levels the loads cover.
    pub fn levels(&self) -> usize {
        usize::from(self.levels)
    }

    /// The level a roll in `0..100` lands on: level 0 takes the first
    /// `load[0]` rolls, level 1 the next `load[1]`, and so on, so each level
    /// takes exactly its share of uniformly drawn rolls. A roll of 100 or
    /// more lands on level 0, as in steer.
    pub fn level_for(&self, roll: u8) -> usize {
        let mut roll = roll;
        for (p, &l) in self.as_slice().iter().enumerate() {
            if roll < l {
                return p;
            }
            roll -= l;
        }
        0
    }
}

/// What the health of a cluster's levels says about where traffic goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Split traffic between the levels by these loads, using only
    /// available endpoints.
    Spread(Loads),
    /// Normalized health is below the panic threshold: ignore health and
    /// pick from every endpoint.
    Panic,
    /// No level has a healthy host and panic routing is off: there is
    /// nowhere to send the request.
    Unavailable,
}

/// Which endpoints a policy may pick from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Candidates {
    /// Every endpoint of the cluster, available or not (panic routing).
    All,
    /// The available endpoints of one priority level.
    Level(usize),
}

impl Decision {
    /// The loads, or `None` in panic or when nothing is available.
    pub fn loads(&self) -> Option<&Loads> {
        match self {
            Decision::Spread(loads) => Some(loads),
            Decision::Panic | Decision::Unavailable => None,
        }
    }

    /// Picks an endpoint the way steer's `LB.Pick` does once it has the
    /// decision. In panic, `pick` is asked once for [`Candidates::All`].
    /// Otherwise `roll` (in `0..100`) chooses a level by its load and
    /// `pick` is asked for that level; if it finds nothing there (every
    /// available host in it was already tried), the other levels are tried
    /// in priority order. Returns what `pick` found, or `None`.
    pub fn route<F>(&self, roll: u8, mut pick: F) -> Option<usize>
    where
        F: FnMut(Candidates) -> Option<usize>,
    {
        match self {
            Decision::Panic => pick(Candidates::All),
            Decision::Unavailable => None,
            Decision::Spread(loads) => {
                let chosen = loads.level_for(roll);
                pick(Candidates::Level(chosen)).or_else(|| {
                    (0..loads.levels())
                        .filter(|&p| p != chosen)
                        .find_map(|p| pick(Candidates::Level(p)))
                })
            }
        }
    }
}

/// The roll a hashing policy uses to choose a level from a request's hash
/// key, so a key keeps its endpoint for as long as the health picture does
/// not change. It uses the key's high half, leaving the low half to the
/// policy.
pub fn hash_roll(key: u64) -> u8 {
    // Below 100, so the narrowing never loses anything.
    ((key >> 32) % 100) as u8
}

/// Computes the share of traffic each priority level receives from the
/// number of healthy and total endpoints in each level, steer's
/// `PriorityLoad`. `panic_threshold` is a percentage; zero disables panic.
///
/// # Panics
///
/// If the slices differ in length or are longer than [`LEVELS`].
pub fn priority_load(healthy: &[u32], total: &[u32], panic_threshold: u8) -> Decision {
    assert_eq!(healthy.len(), total.len(), "one healthy count per level");
    assert!(total.len() <= LEVELS, "at most {LEVELS} priority levels");
    let mut health = [0u32; LEVELS];
    let mut sum = 0;
    for (p, (&h, &t)) in healthy.iter().zip(total).enumerate() {
        if t > 0 {
            let share = OVERPROVISIONING_FACTOR * u64::from(h) / u64::from(t);
            // At most 100, so it fits.
            health[p] = share.min(100) as u32;
        }
        sum += health[p];
    }
    let normalized = sum.min(100);
    if normalized < u32::from(panic_threshold) {
        return Decision::Panic;
    }
    if normalized == 0 {
        return Decision::Unavailable;
    }
    let levels = total.len();
    let mut percent = [0u8; LEVELS];
    let mut remaining = 100;
    for p in 0..levels {
        let load = remaining.min(health[p] * 100 / normalized);
        // At most 100, so it fits.
        percent[p] = load as u8;
        remaining -= load;
    }
    // normalized > 0, so some level has health.
    let first = health[..levels].iter().position(|&h| h > 0).unwrap_or(0);
    percent[first] += remaining as u8;
    Decision::Spread(Loads {
        levels: levels as u8,
        percent,
    })
}

/// The healthy and total endpoint counts of each priority level, gathered
/// one endpoint at a time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Census {
    levels: u8,
    healthy: [u32; LEVELS],
    total: [u32; LEVELS],
}

impl Census {
    /// A census of `levels` empty levels. A cluster has as many levels as
    /// its highest priority plus one, even when a level in between has no
    /// endpoints.
    ///
    /// # Panics
    ///
    /// If `levels` is more than [`LEVELS`].
    pub fn new(levels: usize) -> Census {
        assert!(levels <= LEVELS, "at most {LEVELS} priority levels");
        Census {
            levels: levels as u8,
            ..Census::default()
        }
    }

    /// Counts one endpoint, growing the census to cover its level.
    ///
    /// # Panics
    ///
    /// If `priority` is above
    /// [`MAX_PRIORITY`](edgefront_core::config::limits::MAX_PRIORITY).
    pub fn add(&mut self, priority: u8, available: bool) {
        assert!(priority <= MAX_PRIORITY, "priority {priority} out of range");
        let p = usize::from(priority);
        self.levels = self.levels.max(priority + 1);
        self.total[p] += 1;
        if available {
            self.healthy[p] += 1;
        }
    }

    /// How many levels the census covers.
    pub fn levels(&self) -> usize {
        usize::from(self.levels)
    }

    /// Available endpoints per level.
    pub fn healthy(&self) -> &[u32] {
        &self.healthy[..self.levels()]
    }

    /// Endpoints per level.
    pub fn total(&self) -> &[u32] {
        &self.total[..self.levels()]
    }

    /// The priority decision for these counts.
    pub fn decide(&self, panic_threshold: u8) -> Decision {
        priority_load(self.healthy(), self.total(), panic_threshold)
    }

    /// The priority picture as steer's admin API reports it.
    pub fn state(&self, panic_threshold: u8) -> PriorityState {
        let decision = self.decide(panic_threshold);
        PriorityState {
            healthy: self.healthy().to_vec(),
            total: self.total().to_vec(),
            priority_load: match decision.loads() {
                Some(loads) => loads.as_slice().to_vec(),
                None => vec![0; self.levels()],
            },
            panic: decision == Decision::Panic,
        }
    }
}

impl FromIterator<(u8, bool)> for Census {
    /// Counts `(priority, available)` pairs.
    fn from_iter<I: IntoIterator<Item = (u8, bool)>>(iter: I) -> Census {
        let mut census = Census::default();
        for (priority, available) in iter {
            census.add(priority, available);
        }
        census
    }
}

/// The current priority picture of a cluster, steer's `balancer.State`.
/// The load is all zeros in panic and when nothing is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorityState {
    pub healthy: Vec<u32>,
    pub total: Vec<u32>,
    pub priority_load: Vec<u8>,
    pub panic: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spread(healthy: &[u32], total: &[u32], panic_threshold: u8) -> Vec<u8> {
        match priority_load(healthy, total, panic_threshold) {
            Decision::Spread(loads) => loads.as_slice().to_vec(),
            other => panic!("healthy {healthy:?} total {total:?}: {other:?}"),
        }
    }

    // TestPriorityLoadEnvoyExamples: the examples from Envoy's priority
    // levels documentation (overprovisioning 1.4).
    #[test]
    fn envoy_examples() {
        let cases = [
            // healthy percent of 100 hosts in P0 and P1, then the loads
            (100, 100, 100, 0),
            (72, 100, 100, 0),
            (71, 100, 99, 1),
            (50, 100, 70, 30),
            (25, 100, 35, 65),
            (25, 25, 50, 50),
            (0, 100, 0, 100),
        ];
        for (p0, p1, load0, load1) in cases {
            assert_eq!(
                spread(&[p0, p1], &[100, 100], 0),
                [load0, load1],
                "P0 {p0}% P1 {p1}%"
            );
        }
    }

    // TestPriorityLoadPanicAndEmpty.
    #[test]
    fn panic_and_empty() {
        // 20% healthy in the only level: normalized health 28 < 50.
        assert_eq!(priority_load(&[1], &[5], 50), Decision::Panic);
        // No healthy hosts, panic disabled.
        assert_eq!(priority_load(&[0, 0], &[2, 1], 0), Decision::Unavailable);
        // All down with a threshold must panic.
        assert_eq!(priority_load(&[0], &[3], 1), Decision::Panic);
        // A level with no hosts contributes nothing.
        assert_eq!(spread(&[0, 2, 1], &[0, 2, 1], 50), [0, 100, 0]);
        // Rounding leftovers go to the first level with health: 1/3 of P0
        // healthy (health 46), 1/3 of P1 healthy (46): 92 normalised.
        let load = spread(&[1, 1], &[3, 3], 0);
        assert!(load[0] + load[1] == 100 && load[0] >= load[1], "{load:?}");
        assert_eq!(load, [50, 50]);
    }

    #[test]
    fn panic_threshold_boundary() {
        // Health 70: in panic only above 70.
        assert!(matches!(priority_load(&[1], &[2], 70), Decision::Spread(_)));
        assert_eq!(priority_load(&[1], &[2], 71), Decision::Panic);
        // More health than 100 across levels still normalises to 100.
        assert_eq!(spread(&[3, 3], &[3, 3], 100), [100, 0]);
    }

    #[test]
    fn no_levels_is_unavailable() {
        assert_eq!(priority_load(&[], &[], 0), Decision::Unavailable);
        assert_eq!(priority_load(&[], &[], 1), Decision::Panic);
    }

    #[test]
    #[should_panic(expected = "one healthy count per level")]
    fn mismatched_lengths_panic() {
        priority_load(&[1], &[1, 1], 0);
    }

    #[test]
    #[should_panic(expected = "at most 8 priority levels")]
    fn too_many_levels_panic() {
        priority_load(&[1; 9], &[1; 9], 0);
    }

    #[test]
    fn level_for_takes_rolls_in_order() {
        let Decision::Spread(loads) = priority_load(&[1, 2, 1], &[2, 2, 1], 0) else {
            panic!("expected loads");
        };
        // Health 70, 100, 100: normalized 100, loads 70, 30, 0.
        assert_eq!(loads.as_slice(), [70, 30, 0]);
        assert_eq!(loads.levels(), 3);
        assert_eq!(loads.level_for(0), 0);
        assert_eq!(loads.level_for(69), 0);
        assert_eq!(loads.level_for(70), 1);
        assert_eq!(loads.level_for(99), 1);
        // Out-of-range rolls land on level 0, as in steer.
        assert_eq!(loads.level_for(100), 0);
        assert_eq!(loads.level_for(u8::MAX), 0);
    }

    #[test]
    fn hash_roll_uses_the_high_half() {
        assert_eq!(hash_roll(0), 0);
        assert_eq!(hash_roll(u64::from(u32::MAX)), 0);
        assert_eq!(hash_roll(42 << 32), 42);
        assert_eq!(hash_roll(142 << 32 | 7), 42);
        assert_eq!(hash_roll(u64::MAX), (u64::from(u32::MAX) % 100) as u8);
    }

    #[test]
    fn census_counts_levels() {
        let census: Census = [(0, true), (0, false), (2, true)].into_iter().collect();
        assert_eq!(census.levels(), 3);
        assert_eq!(census.healthy(), [1, 0, 1]);
        assert_eq!(census.total(), [2, 0, 1]);
        let mut wide = Census::new(4);
        wide.add(1, true);
        assert_eq!(wide.levels(), 4);
        assert_eq!(wide.total(), [0, 1, 0, 0]);
        assert_eq!(Census::new(0).levels(), 0);
    }

    #[test]
    #[should_panic(expected = "priority 8 out of range")]
    fn census_rejects_a_priority_out_of_range() {
        Census::default().add(MAX_PRIORITY + 1, true);
    }

    #[test]
    #[should_panic(expected = "at most 8 priority levels")]
    fn census_rejects_too_many_levels() {
        Census::new(LEVELS + 1);
    }

    // The state halves of TestLBPriorityFailover and TestLBPanicRoutesToEveryone.
    #[test]
    fn state_reports_loads_panic_and_zeros() {
        // Two primaries, one down, and a backup: a 70/30 split.
        let census: Census = [(0, false), (0, true), (1, true)].into_iter().collect();
        assert_eq!(
            census.state(50),
            PriorityState {
                healthy: vec![1, 1],
                total: vec![2, 1],
                priority_load: vec![70, 30],
                panic: false,
            }
        );
        // One of five healthy: health 28 < 50.
        let census: Census = [(0, false), (0, false), (0, false), (0, false), (0, true)]
            .into_iter()
            .collect();
        let st = census.state(50);
        assert!(st.panic);
        assert_eq!(st.priority_load, [0]);
        // Without a panic threshold the last healthy host takes everything,
        // and with none left there is no load at all.
        assert_eq!(census.state(0).priority_load, [100]);
        let census: Census = [(0, false)].into_iter().collect();
        let st = census.state(0);
        assert!(!st.panic);
        assert_eq!(st.priority_load, [0]);
    }

    /// The candidate lists of a census, as steer's `LB.Pick` builds them:
    /// every endpoint, and the available ones per level.
    struct Cluster {
        all: Vec<usize>,
        levels: Vec<Vec<usize>>,
    }

    impl Cluster {
        fn new(endpoints: &[(u8, bool)]) -> (Cluster, Census) {
            let census: Census = endpoints.iter().copied().collect();
            let mut levels = vec![Vec::new(); census.levels()];
            for (i, &(p, available)) in endpoints.iter().enumerate() {
                if available {
                    levels[usize::from(p)].push(i);
                }
            }
            let all = (0..endpoints.len()).collect();
            (Cluster { all, levels }, census)
        }

        fn candidates(&self, c: Candidates) -> &[usize] {
            match c {
                Candidates::All => &self.all,
                Candidates::Level(p) => &self.levels[p],
            }
        }
    }

    // The routing half of TestLBPriorityFailover, with a roll for every
    // percent instead of a random one, so the shares are exact.
    #[test]
    fn route_follows_the_loads() {
        let share = |endpoints: &[(u8, bool)]| {
            let (cluster, census) = Cluster::new(endpoints);
            let decision = census.decide(50);
            let mut counts = vec![0; endpoints.len()];
            let mut next = 0;
            for roll in 0..100 {
                let i = decision.route(roll, |c| {
                    let cands = cluster.candidates(c);
                    next += 1;
                    cands.get(next % cands.len().max(1)).copied()
                });
                counts[i.expect("an endpoint")] += 1;
            }
            counts
        };
        // All healthy: the backup gets nothing.
        assert_eq!(share(&[(0, true), (0, true), (1, true)])[2], 0);
        // One primary down: P0 health 70, a 70/30 split.
        assert_eq!(share(&[(0, false), (0, true), (1, true)]), [0, 70, 30]);
        // Both primaries out: everything to P1.
        assert_eq!(share(&[(0, false), (0, false), (1, true)]), [0, 0, 100]);
    }

    // The routing half of TestLBPanicRoutesToEveryone.
    #[test]
    fn panic_routes_to_everyone() {
        let endpoints = [(0, false), (0, false), (0, false), (0, false), (0, true)];
        let (cluster, census) = Cluster::new(&endpoints);
        let mut asked = Vec::new();
        let decision = census.decide(50);
        assert_eq!(decision, Decision::Panic);
        assert_eq!(decision.loads(), None);
        let got = decision.route(0, |c| {
            asked.push(c);
            cluster.candidates(c).first().copied()
        });
        assert_eq!(got, Some(0));
        assert_eq!(asked, [Candidates::All]);
        // Without a threshold the healthy host takes everything.
        let decision = census.decide(0);
        for roll in 0..100 {
            let got = decision.route(roll, |c| cluster.candidates(c).first().copied());
            assert_eq!(got, Some(4));
        }
        // And with it gone there is nowhere to go; the policy is not asked.
        let (_, census) = Cluster::new(&[(0, false); 5]);
        let got = census.decide(0).route(0, |_| panic!("asked to pick"));
        assert_eq!(got, None);
    }

    // The fallback half of TestLBNeverRepeatsAndFallsBack: once every
    // available host of the chosen level is tried, the other levels are
    // tried in priority order.
    #[test]
    fn route_falls_back_in_priority_order() {
        let endpoints = [(0, true), (1, true), (2, true), (3, true)];
        let (cluster, _) = Cluster::new(&endpoints);
        // A level with no health still exists for fallback; take loads
        // from health that puts everything on P2.
        let decision = priority_load(&[0, 0, 1, 0], &[1, 1, 1, 1], 0);
        assert_eq!(
            decision.loads().map(Loads::as_slice),
            Some(&[0, 0, 100, 0][..])
        );
        let mut tried = Vec::new();
        for _ in 0..4 {
            let mut asked = Vec::new();
            let got = decision.route(17, |c| {
                asked.push(c);
                cluster
                    .candidates(c)
                    .iter()
                    .copied()
                    .find(|i| !tried.contains(i))
            });
            tried.push(got.expect("an untried endpoint"));
            assert_eq!(asked[0], Candidates::Level(2));
        }
        assert_eq!(tried, [2, 0, 1, 3]);
        let mut asked = Vec::new();
        let got = decision.route(17, |c| {
            asked.push(c);
            None
        });
        assert_eq!(got, None);
        assert_eq!(
            asked,
            [2, 0, 1, 3].map(Candidates::Level),
            "chosen level first, then the rest in order"
        );
    }
}
