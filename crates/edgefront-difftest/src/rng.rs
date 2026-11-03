//! A small seeded generator, so that every generated case can be replayed
//! from the seed printed with a run.

/// SplitMix64: one 64-bit word of state, statistically good enough for
/// test generation, and the same sequence on every platform.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number in `0..n`. `n` must not be zero.
    pub fn below(&mut self, n: usize) -> usize {
        assert!(n > 0, "below(0)");
        // Lemire's multiply-shift; the bias is far below what a test needs.
        ((u128::from(self.next_u64()) * n as u128) >> 64) as usize
    }

    /// A number in `lo..=hi`.
    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    /// True with probability `p`, for `p` in `0.0..=1.0`.
    pub fn chance(&mut self, p: f64) -> bool {
        ((self.next_u64() >> 11) as f64 / (1u64 << 53) as f64) < p
    }

    /// One element of a non-empty slice.
    pub fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }

    /// A child generator whose sequence depends only on this one's next word,
    /// so a case can be regenerated without replaying the others.
    pub fn fork(&mut self) -> Rng {
        Rng(self.next_u64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let a: Vec<u64> = (0..8)
            .scan(Rng::new(42), |r, _| Some(r.next_u64()))
            .collect();
        let b: Vec<u64> = (0..8)
            .scan(Rng::new(42), |r, _| Some(r.next_u64()))
            .collect();
        assert_eq!(a, b);
        // The reference SplitMix64 output for seed 0.
        assert_eq!(Rng::new(0).next_u64(), 0xe220_a839_7b1d_cdaf);
    }

    #[test]
    fn below_and_range_stay_in_bounds_and_cover_them() {
        let mut r = Rng::new(1);
        let mut seen = [0usize; 7];
        for _ in 0..7_000 {
            seen[r.below(7)] += 1;
            let v = r.range(3, 5);
            assert!((3..=5).contains(&v));
        }
        assert!(seen.iter().all(|&n| n > 800), "{seen:?}");
    }

    #[test]
    #[should_panic(expected = "below(0)")]
    fn below_zero_panics() {
        Rng::new(1).below(0);
    }

    #[test]
    fn chance_tracks_its_probability() {
        let mut r = Rng::new(9);
        let hits = (0..10_000).filter(|_| r.chance(0.25)).count();
        assert!((2_200..2_800).contains(&hits), "{hits}");
        assert!(!(0..1000).any(|_| r.chance(0.0)));
        assert!((0..1000).all(|_| r.chance(1.0)));
    }

    #[test]
    fn forks_are_independent_of_later_draws() {
        let mut a = Rng::new(5);
        let mut b = Rng::new(5);
        let mut fa = a.fork();
        let mut fb = b.fork();
        a.next_u64();
        assert_eq!(fa.next_u64(), fb.next_u64());
        assert_eq!(a.pick(&["x"]), "x");
    }
}
