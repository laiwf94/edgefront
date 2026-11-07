//! The random source of the balancing policies.
//!
//! steer's balancers take `func(n int) int` and default to math/rand/v2.
//! Here a policy takes an [`Rng`], and the one provided, [`SplitMix64`], is
//! the same generator steer's differential driver hands the Go balancers
//! (`legacy/steer/cmd/diffdrive/splitmix.go`): given one seed, the Rust and
//! Go policies draw the same numbers and so make the same picks, which is
//! what lets their pick sequences be diffed.

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A source of uniform draws, shared by every request of a cluster.
pub trait Rng: Send + Sync + fmt::Debug {
    /// A uniform number in `0..n`.
    ///
    /// # Panics
    ///
    /// If `n` is zero.
    fn below(&self, n: usize) -> usize;
}

/// SplitMix64 (Steele, Lea and Flood), with Lemire's multiply-shift for
/// bounded draws.
///
/// Its state advances by a constant on every draw, so the step is a single
/// atomic add: draws from many threads at once never lose or repeat a state,
/// and a single thread sees exactly the sequence of the seed.
pub struct SplitMix64 {
    state: AtomicU64,
}

const GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

impl SplitMix64 {
    /// A generator that replays the sequence of `seed`.
    pub fn new(seed: u64) -> SplitMix64 {
        SplitMix64 {
            state: AtomicU64::new(seed),
        }
    }

    /// A generator seeded from the process's random hasher keys and the
    /// time, for production use where picks need not be replayed.
    pub fn from_entropy() -> SplitMix64 {
        let mut h = RandomState::new().build_hasher();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        h.write_u128(nanos);
        SplitMix64::new(h.finish())
    }

    /// The next 64-bit output.
    pub fn next_u64(&self) -> u64 {
        let z = self
            .state
            .fetch_add(GAMMA, Ordering::Relaxed)
            .wrapping_add(GAMMA);
        let z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        let z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

impl Rng for SplitMix64 {
    fn below(&self, n: usize) -> usize {
        assert!(n > 0, "below(0)");
        // The high word of the product is below n, so it fits.
        ((u128::from(self.next_u64()) * n as u128) >> 64) as usize
    }
}

impl fmt::Debug for SplitMix64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The state is the next draw; keep it out of logs.
        f.debug_struct("SplitMix64").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    #[test]
    fn reference_outputs() {
        // The published SplitMix64 outputs for seed 0.
        let s = SplitMix64::new(0);
        assert_eq!(s.next_u64(), 0xe220_a839_7b1d_cdaf);
        assert_eq!(s.next_u64(), 0x6e78_9e6a_a1b9_65f4);
        assert_eq!(s.next_u64(), 0x06c4_5d18_8009_454f);
    }

    #[test]
    fn below_stays_in_bounds_and_covers_them() {
        let s = SplitMix64::new(7);
        let mut seen = [0u32; 7];
        for _ in 0..7_000 {
            seen[s.below(7)] += 1;
        }
        assert!(seen.iter().all(|&n| n > 850), "{seen:?}");
        assert!((0..100).all(|_| s.below(1) == 0));
        assert!(s.below(usize::MAX) < usize::MAX);
    }

    #[test]
    #[should_panic(expected = "below(0)")]
    fn below_zero_panics() {
        SplitMix64::new(1).below(0);
    }

    #[test]
    fn concurrent_draws_take_every_state_once() {
        let shared = Arc::new(SplitMix64::new(99));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = Arc::clone(&shared);
                thread::spawn(move || (0..1_000).map(|_| s.next_u64()).collect::<Vec<_>>())
            })
            .collect();
        let mut got: Vec<u64> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        got.sort_unstable();
        let reference = SplitMix64::new(99);
        let mut want: Vec<u64> = (0..8_000).map(|_| reference.next_u64()).collect();
        want.sort_unstable();
        assert_eq!(got, want);
    }

    #[test]
    fn entropy_seeds_differ_and_debug_hides_the_state() {
        let a = SplitMix64::from_entropy();
        let b = SplitMix64::from_entropy();
        assert_ne!(a.next_u64(), b.next_u64());
        assert_eq!(format!("{a:?}"), "SplitMix64 { .. }");
    }
}
