//! Smooth weighted round robin, ported from steer's `balancer.RoundRobin`.
//!
//! This is nginx's algorithm. Each pick adds every candidate's weight to its
//! running score, takes the highest score and subtracts the total weight of
//! the candidates from it. Over any run of picks with a fixed candidate set,
//! each endpoint is chosen in proportion to its weight and the choices are
//! spread out: weights 5, 1, 1 give `a a b a c a a`, not `a a a a a b c`.
//! Ties go to the candidate listed first.

use std::sync::{Mutex, PoisonError};

use super::Policy;

/// The longest cycle that is recorded for replay, in picks. A cycle is as
/// long as the total weight of its candidates; 16,384 picks of two bytes
/// each cover 100 endpoints of average weight 163 in 32 KiB.
pub const MAX_CYCLE: usize = 1 << 14;

/// A smooth weighted round robin over a fixed set of endpoints.
///
/// Picking scans every candidate, which costs O(n). But with a fixed
/// candidate set the picks repeat: once the scores over the set come back to
/// where they were total-weight picks earlier, the same picks follow forever.
/// So while first attempts keep coming with the same candidates, the picks
/// of one such cycle are recorded and then replayed in O(1). Any other pick
/// (another candidate set, or a retry that skips a candidate) first turns
/// the replay position back into scores, so every pick is exactly the one
/// steer's scan would make.
#[derive(Debug)]
pub struct RoundRobin {
    weights: Box<[i64]>,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// The running score of every endpoint. While replaying, the scores as
    /// of the start of the cycle.
    current: Box<[i64]>,
    /// The candidate set being recorded or replayed.
    set: Vec<usize>,
    /// The total weight of `set`, which is the length of its cycle.
    total: i64,
    /// The scores of `set` when recording started.
    start: Vec<i64>,
    /// The picks recorded since then.
    picks: Vec<u16>,
    phase: Phase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Nothing is recorded.
    Idle,
    /// The picks of `set` since `start` are being recorded.
    Recording,
    /// `picks` is a full cycle from `current`; the next pick is `picks[pos]`.
    Replaying { pos: usize },
}

impl RoundRobin {
    /// A round robin over endpoints with these weights, indexed as given.
    pub fn new(weights: &[u32]) -> RoundRobin {
        RoundRobin {
            weights: weights.iter().map(|&w| i64::from(w)).collect(),
            state: Mutex::new(State {
                current: vec![0; weights.len()].into(),
                set: Vec::new(),
                total: 0,
                start: Vec::new(),
                picks: Vec::new(),
                phase: Phase::Idle,
            }),
        }
    }

    /// How many endpoints the round robin covers.
    pub fn len(&self) -> usize {
        self.weights.len()
    }

    /// Whether it covers no endpoints.
    pub fn is_empty(&self) -> bool {
        self.weights.is_empty()
    }

    /// Whether picks over the current candidate set are being replayed
    /// rather than scanned.
    pub fn replaying(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        matches!(state.phase, Phase::Replaying { .. })
    }

    /// steer's pick: add every open candidate's weight to its score, take
    /// the first highest score and take the total off it.
    fn scan(&self, current: &mut [i64], cands: &[usize], tried: &[usize]) -> Scan {
        let mut best = None;
        let mut best_score = 0;
        let mut total = 0;
        let mut skipped = false;
        for &i in cands {
            if !tried.is_empty() && tried.contains(&i) {
                skipped = true;
                continue;
            }
            let w = self.weights[i];
            let score = current[i] + w;
            current[i] = score;
            total += w;
            if best.is_none() || score > best_score {
                best = Some(i);
                best_score = score;
            }
        }
        if let Some(i) = best {
            current[i] -= total;
        }
        Scan {
            best,
            total,
            skipped,
        }
    }
}

/// What one scan found.
struct Scan {
    best: Option<usize>,
    /// The total weight of the open candidates.
    total: i64,
    /// Whether a candidate was skipped as tried.
    skipped: bool,
}

impl State {
    /// Turns a replay position back into scores: `pos` picks into the cycle,
    /// each endpoint of the set has gained its weight `pos` times and each
    /// pick has cost its endpoint the total.
    fn materialize(&mut self, weights: &[i64]) {
        if let Phase::Replaying { pos } = self.phase {
            let steps = pos as i64;
            for &i in &self.set {
                self.current[i] += steps * weights[i];
            }
            for &p in &self.picks[..pos] {
                self.current[usize::from(p)] -= self.total;
            }
        }
        self.phase = Phase::Idle;
    }

    /// Starts recording the cycle of `cands`, whose total weight is `total`,
    /// from the current scores.
    fn record(&mut self, cands: &[usize], total: i64) {
        self.set.clear();
        self.set.extend_from_slice(cands);
        self.total = total;
        self.start.clear();
        self.start.extend(cands.iter().map(|&i| self.current[i]));
        self.picks.clear();
        self.phase = Phase::Recording;
    }

    /// Whether the scores of the set are back where recording started.
    fn back_at_start(&self) -> bool {
        self.set
            .iter()
            .zip(&self.start)
            .all(|(&i, &s)| self.current[i] == s)
    }
}

impl Policy for RoundRobin {
    /// Picks among `cands`, which must be distinct indices, never one in
    /// `tried`. Endpoints left out of a pick keep their score for the next.
    fn pick(&self, cands: &[usize], tried: &[usize], _key: Option<u64>) -> Option<usize> {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let state = &mut *guard;
        if let Phase::Replaying { pos } = state.phase {
            if tried.is_empty() && state.set == cands {
                let next = pos + 1;
                state.phase = Phase::Replaying {
                    pos: if next == state.picks.len() { 0 } else { next },
                };
                return Some(usize::from(state.picks[pos]));
            }
            state.materialize(&self.weights);
        }
        let scan = self.scan(&mut state.current, cands, tried);
        let Some(pick) = scan.best else {
            state.phase = Phase::Idle;
            return None;
        };
        // A pick that skips nothing is the same as a first attempt.
        if scan.skipped {
            state.phase = Phase::Idle;
        } else if state.phase == Phase::Recording && state.set == cands {
            // Recording only starts for a round robin of at most
            // u16::MAX + 1 endpoints, so every index fits.
            state.picks.push(pick as u16);
            if state.picks.len() as i64 == state.total {
                if state.back_at_start() {
                    state.phase = Phase::Replaying { pos: 0 };
                } else {
                    // Not periodic from there yet; try again from here.
                    state.record(cands, scan.total);
                }
            }
        } else if scan.total > 0
            && scan.total as usize <= MAX_CYCLE
            && self.weights.len() <= usize::from(u16::MAX) + 1
        {
            state.record(cands, scan.total);
        } else {
            state.phase = Phase::Idle;
        }
        Some(pick)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::balancer::rng::{Rng, SplitMix64};

    fn all(n: usize) -> Vec<usize> {
        (0..n).collect()
    }

    // TestSmoothRoundRobinSequence.
    #[test]
    fn smooth_sequence() {
        let rr = RoundRobin::new(&[5, 1, 1]);
        let got: String = (0..7)
            .map(|_| char::from(b"abc"[rr.pick(&[0, 1, 2], &[], None).unwrap()]))
            .collect();
        assert_eq!(got, "aabacaa", "nginx smooth weighted round robin");
    }

    // TestSmoothRoundRobinMatchesReference: in every window of sum(weights)
    // consecutive picks over a fixed set, each endpoint is chosen exactly
    // weight times, and no endpoint waits more than twice its fair interval
    // between turns.
    #[test]
    fn matches_reference() {
        let rng = SplitMix64::new(34);
        for _ in 0..300 {
            let n = 1 + rng.below(8);
            let weights: Vec<u32> = (0..n).map(|_| 1 + rng.below(10) as u32).collect();
            let total: usize = weights.iter().map(|&w| w as usize).sum();
            let rr = RoundRobin::new(&weights);
            let cands = all(n);
            let mut last: Vec<Option<usize>> = vec![None; n];
            for window in 0..4 {
                let mut counts = vec![0u32; n];
                for k in 0..total {
                    let pos = window * total + k;
                    let i = rr.pick(&cands, &[], None).unwrap();
                    counts[i] += 1;
                    let w = weights[i] as usize;
                    if let Some(l) = last[i] {
                        assert!(
                            pos - l <= 2 * total.div_ceil(w),
                            "weights {weights:?}: endpoint {i} waited {} picks",
                            pos - l
                        );
                    }
                    last[i] = Some(pos);
                }
                assert_eq!(counts, weights, "window {window}");
            }
        }
    }

    // TestRoundRobinSkip.
    #[test]
    fn skips_tried() {
        let rr = RoundRobin::new(&[1, 1]);
        for _ in 0..10 {
            assert_eq!(rr.pick(&[0, 1], &[0], None), Some(1));
        }
        assert_eq!(rr.pick(&[0, 1], &[0, 1], None), None);
        assert_eq!(rr.pick(&[], &[], None), None);
    }

    #[test]
    fn subsets_keep_scores() {
        // Endpoint 2 sits out two picks and keeps its score of zero, so it
        // is not owed a burst when it returns.
        let rr = RoundRobin::new(&[1, 1, 1]);
        assert_eq!(rr.pick(&[0, 1], &[], None), Some(0));
        assert_eq!(rr.pick(&[0, 1], &[], None), Some(1));
        assert_eq!(rr.pick(&[0, 1, 2], &[], None), Some(0));
        assert_eq!(rr.pick(&[0, 1, 2], &[], None), Some(1));
        assert_eq!(rr.pick(&[0, 1, 2], &[], None), Some(2));
        assert_eq!(rr.len(), 3);
        assert!(!rr.is_empty());
        assert!(RoundRobin::new(&[]).is_empty());
    }

    /// steer's `RoundRobin.Pick`, line for line, as the reference.
    struct Reference {
        weights: Vec<i64>,
        current: Vec<i64>,
    }

    impl Reference {
        fn pick(&mut self, cands: &[usize], tried: &[usize]) -> Option<usize> {
            let mut best: Option<usize> = None;
            let mut total = 0;
            for &i in cands {
                if tried.contains(&i) {
                    continue;
                }
                self.current[i] += self.weights[i];
                total += self.weights[i];
                if best.is_none_or(|b| self.current[i] > self.current[b]) {
                    best = Some(i);
                }
            }
            if let Some(b) = best {
                self.current[b] -= total;
            }
            best
        }
    }

    /// Long runs over one candidate set, so cycles are recorded and
    /// replayed, broken by other sets and by retries at every position.
    #[test]
    fn replay_matches_the_scan() {
        let rng = SplitMix64::new(2026);
        let mut replayed = 0;
        for case in 0..400 {
            let n = 1 + rng.below(12);
            let weights: Vec<u32> = (0..n)
                .map(|_| match rng.below(10) {
                    0 => rng.below(3) as u32, // zero weights too
                    1 => 1 + rng.below(1000) as u32,
                    _ => 1 + rng.below(10) as u32,
                })
                .collect();
            let rr = RoundRobin::new(&weights);
            let mut reference = Reference {
                weights: weights.iter().map(|&w| i64::from(w)).collect(),
                current: vec![0; n],
            };
            let mut set: Vec<usize> = all(n);
            for step in 0..3_000 {
                let (cands, tried) = match rng.below(100) {
                    0..3 => {
                        set = (0..n).filter(|_| rng.below(3) > 0).collect();
                        (set.clone(), vec![])
                    }
                    3..6 => (set.clone(), vec![rng.below(n)]),
                    6..8 => (all(n), vec![]),
                    8..9 => (set.clone(), vec![n + 5]), // tries nothing listed
                    _ => (set.clone(), vec![]),
                };
                let want = reference.pick(&cands, &tried);
                let got = rr.pick(&cands, &tried, None);
                assert_eq!(
                    got, want,
                    "case {case} step {step}: weights {weights:?} cands {cands:?} tried {tried:?}"
                );
                replayed += usize::from(rr.replaying());
            }
        }
        assert!(replayed > 100_000, "replay engaged on {replayed} picks");
    }

    #[test]
    fn replays_after_one_cycle_and_leaves_on_change() {
        let rr = RoundRobin::new(&[5, 1, 1]);
        let cands = [0, 1, 2];
        // The first pick starts recording, the next seven fill the cycle.
        for _ in 0..8 {
            assert!(!rr.replaying());
            rr.pick(&cands, &[], None);
        }
        assert!(rr.replaying());
        let got: String = (0..14)
            .map(|_| char::from(b"abc"[rr.pick(&cands, &[], None).unwrap()]))
            .collect();
        // Eight picks in: one into the cycle aabacaa, which then repeats.
        assert_eq!(got, "abacaaaabacaaa");
        assert_eq!(rr.pick(&cands, &[0], None), Some(1));
        assert!(!rr.replaying());
        // Too long a cycle is never recorded.
        let big = RoundRobin::new(&[1000; 17]);
        let cands: Vec<usize> = (0..17).collect();
        for _ in 0..20_000 {
            big.pick(&cands, &[], None);
        }
        assert!(!big.replaying());
    }

    #[test]
    fn survives_a_poisoned_lock() {
        let rr = std::sync::Arc::new(RoundRobin::new(&[1, 1]));
        let r = std::sync::Arc::clone(&rr);
        let _ = std::thread::spawn(move || {
            let _guard = r.state.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert!(rr.pick(&[0, 1], &[], None).is_some());
    }
}
