//! Hot keys: a count-min sketch (Cormode and Muthukrishnan, 2005) of this
//! node's requests, aged so it measures a rate.
//!
//! Every request adds one to its key's counter in each of `DEPTH` rows; a
//! key's count is the smallest of its counters, which can only overestimate,
//! and by little when the sketch is wide. Every `window` all counters are
//! halved, so a key requested `r` times a second settles near `2 r window`.
//! A key whose count reaches the threshold for `hot_rate` is hot, and a hot
//! key that falls below half of it is demoted, so a key near the threshold
//! does not flap.
//!
//! A node keeps its own copy of a hot key instead of fetching it from the
//! owner each time (`tier` stores peer-fetched responses only for hot keys),
//! and drops the copy when the key is demoted.

use std::collections::HashSet;
use std::time::Duration;

const WIDTH: usize = 4096;
const DEPTH: usize = 4;
/// Hot keys tracked at most; a key past this is counted but not promoted.
pub const MAX_HOT: usize = 1024;

/// What one request did to its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    None,
    /// The key just became hot.
    Promoted,
}

pub struct HotKeys {
    counters: Vec<u32>,
    window_ms: u64,
    next_halving: u64,
    promote_at: u32,
    hot: HashSet<String>,
}

fn hash(key: &str, row: usize) -> usize {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ (row as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    for b in key.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    ((h ^ (h >> 31)) as usize) % WIDTH
}

impl HotKeys {
    /// Keys at `hot_rate` requests a second or more are hot, measured over
    /// halvings every `window`.
    pub fn new(hot_rate: f64, window: Duration) -> Self {
        let window_ms = window.as_millis().max(1) as u64;
        let promote_at = (2.0 * hot_rate * window_ms as f64 / 1000.0).ceil().max(2.0);
        HotKeys {
            counters: vec![0; WIDTH * DEPTH],
            window_ms,
            next_halving: window_ms,
            promote_at: promote_at.min(f64::from(u32::MAX)) as u32,
            hot: HashSet::new(),
        }
    }

    /// Halves every counter for each window passed by `now` (ms), and
    /// returns the keys that fell below half the threshold: demoted.
    pub fn age(&mut self, now: u64) -> Vec<String> {
        let mut halvings = 0;
        while now >= self.next_halving && halvings < 32 {
            for c in &mut self.counters {
                *c /= 2;
            }
            self.next_halving += self.window_ms;
            halvings += 1;
        }
        if now >= self.next_halving {
            // Idle for over 32 windows: everything is zero anyway.
            self.next_halving = now - now % self.window_ms + self.window_ms;
        }
        if halvings == 0 {
            return Vec::new();
        }
        let demote_below = self.promote_at / 2;
        let gone: Vec<String> = self
            .hot
            .iter()
            .filter(|k| self.count(k) < demote_below)
            .cloned()
            .collect();
        for k in &gone {
            self.hot.remove(k);
        }
        gone
    }

    /// The sketch's count for `key`.
    pub fn count(&self, key: &str) -> u32 {
        (0..DEPTH)
            .map(|row| self.counters[row * WIDTH + hash(key, row)])
            .min()
            .unwrap_or(0)
    }

    /// Counts one request for `key` at `now` (ms).
    pub fn observe(&mut self, key: &str, now: u64) -> Change {
        self.age(now);
        for row in 0..DEPTH {
            let c = &mut self.counters[row * WIDTH + hash(key, row)];
            *c = c.saturating_add(1);
        }
        if self.count(key) >= self.promote_at
            && self.hot.len() < MAX_HOT
            && self.hot.insert(key.to_owned())
        {
            return Change::Promoted;
        }
        Change::None
    }

    pub fn is_hot(&self, key: &str) -> bool {
        self.hot.contains(key)
    }

    pub fn hot_count(&self) -> usize {
        self.hot.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_past_the_rate_is_promoted_and_demoted_when_it_cools() {
        // 10 requests a second, measured over 1 s windows: hot at 20.
        let mut h = HotKeys::new(10.0, Duration::from_secs(1));
        let mut promoted_at = None;
        // 50 requests a second for 2 s.
        for i in 0..100u64 {
            if h.observe("hot", i * 20) == Change::Promoted {
                promoted_at = Some(i);
            }
            // A cold key at 2 a second alongside.
            if i % 25 == 0 {
                assert_eq!(h.observe("cold", i * 20), Change::None);
            }
        }
        assert!(promoted_at.is_some_and(|i| i < 30), "{promoted_at:?}");
        assert!(h.is_hot("hot") && !h.is_hot("cold"));
        assert_eq!(h.hot_count(), 1);
        // Promoted once only.
        assert_eq!(h.observe("hot", 2_000), Change::None);
        // Silence: halvings bring it below half the threshold, 10.
        assert!(h.age(2_500).is_empty());
        let mut demoted = Vec::new();
        for t in [3_000, 4_000, 5_000, 6_000] {
            demoted.extend(h.age(t));
        }
        assert_eq!(demoted, ["hot"]);
        assert!(!h.is_hot("hot"));
        // A long idle spell empties the sketch.
        h.observe("x", 10_000);
        h.age(1_000_000);
        assert_eq!(h.count("x"), 0);
        assert_eq!(h.age(1_000_000), Vec::<String>::new());
    }

    #[test]
    fn the_sketch_overestimates_little_among_many_keys() {
        let mut h = HotKeys::new(1e9, Duration::from_secs(3600));
        for i in 0..20_000 {
            h.observe(&format!("k{i}"), 0);
        }
        for _ in 0..500 {
            h.observe("popular", 0);
        }
        // Count-min's bound: each row overcounts by the other keys sharing
        // its counter, N / WIDTH on average (5 here), and the minimum of
        // four rows by less.
        let n = 20_500 / WIDTH as u32;
        let popular = h.count("popular");
        assert!((500..=500 + 4 * n).contains(&popular), "{popular}");
        let over: u32 = (0..1000).map(|i| h.count(&format!("k{i}")) - 1).sum();
        assert!(over < 1000 * n, "{over} extra over 1,000 keys");
    }

    #[test]
    fn the_hot_set_is_bounded() {
        let mut h = HotKeys::new(1.0, Duration::from_secs(1));
        for i in 0..(MAX_HOT + 10) {
            let k = format!("k{i}");
            h.observe(&k, 0);
            h.observe(&k, 0);
        }
        assert_eq!(h.hot_count(), MAX_HOT);
    }
}
