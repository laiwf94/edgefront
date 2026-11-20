//! The penalty box: a client that keeps hitting its limit is not
//! evaluated at all for a while.
//!
//! A limit only refuses each request over it; a client that keeps
//! sending at ten times its rate keeps costing a table lookup per
//! request. [`PenaltyBox`] watches a limit's refusals per key (the same
//! key as the limit's, so a client boxed by its API key is boxed by that
//! key) and, once a key has been refused in `windows` distinct windows of
//! `window_ns` in a row, boxes it for the mitigation `timeout_ns`. While
//! boxed it is refused by [`PenaltyBox::check`] before the limit's table
//! is touched. A key boxed again within `forget_ns` of its last release
//! is a repeat offender: its timeout doubles each time, up to
//! `max_timeout_ns`; one that stays clean for `forget_ns` starts over.
//!
//! The box is a fixed-size open-addressed array of 64-byte entries in
//! shards, each entry the key's 64-bit SipHash and its state, so
//! [`PenaltyBox::check`] is one hash, one shard lock and a probe of a few
//! adjacent slots: well under 200 ns. Two keys whose hashes collide share
//! a fate, one chance in 2^64 per pair. When a shard is full the entry
//! with the earliest end is replaced, so a flood of fresh keys can only
//! let the boxed out early, never box anyone who was not refused.

use std::sync::{Mutex, PoisonError};

use crate::siphash::{KEY_LEN, random_key, siphash13};

/// How a [`PenaltyBox`] decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PenaltyConfig {
    /// Refused in this many consecutive windows boxes the key.
    pub windows: u32,
    pub window_ns: u64,
    /// The first mitigation timeout.
    pub timeout_ns: u64,
    /// Repeat offences double the timeout up to this.
    pub max_timeout_ns: u64,
    /// Clean for this long after release, a key starts over.
    pub forget_ns: u64,
    /// Keys tracked at most.
    pub capacity: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct Entry {
    hash: u64,
    /// The window of the last refusal, counted in `window_ns` from 0.
    last_window: u64,
    /// Consecutive windows with a refusal.
    streak: u32,
    /// Times boxed without a clean `forget_ns` in between.
    offences: u32,
    /// Boxed until this time; 0 when not boxed.
    until_ns: u64,
    used: bool,
}

/// What [`PenaltyBox::refused`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strike {
    /// Counted; the key is not boxed.
    Counted { streak: u32 },
    /// The key is now boxed until this time, for its `offence`-th time.
    Boxed { until_ns: u64, offence: u32 },
}

const PROBE: usize = 8;

/// Boxed keys, by hash.
pub struct PenaltyBox {
    cfg: PenaltyConfig,
    hash_key: [u8; KEY_LEN],
    shards: Vec<Mutex<Vec<Entry>>>,
}

impl std::fmt::Debug for PenaltyBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PenaltyBox")
            .field("cfg", &self.cfg)
            .finish()
    }
}

impl PenaltyBox {
    pub fn new(cfg: PenaltyConfig) -> Self {
        let nshards = 64usize;
        let per = cfg
            .capacity
            .div_ceil(nshards)
            .max(PROBE)
            .next_power_of_two();
        PenaltyBox {
            cfg: PenaltyConfig {
                windows: cfg.windows.max(1),
                window_ns: cfg.window_ns.max(1),
                ..cfg
            },
            hash_key: random_key(),
            shards: (0..nshards)
                .map(|_| Mutex::new(vec![Entry::default(); per]))
                .collect(),
        }
    }

    pub fn config(&self) -> &PenaltyConfig {
        &self.cfg
    }

    /// Runs `f` on `key`'s shard with the key's hash and its slot, if
    /// it has one.
    fn slot<R>(&self, key: &[u8], f: impl FnOnce(&mut [Entry], Option<usize>, u64) -> R) -> R {
        let h = siphash13(&self.hash_key, key) | 1;
        let mut s = self.shards[(h >> 58) as usize % self.shards.len()]
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let n = s.len();
        let home = (h as usize) & (n - 1);
        let at = (0..PROBE)
            .map(|i| (home + i) & (n - 1))
            .find(|&i| s[i].used && s[i].hash == h);
        f(&mut s, at, h)
    }

    /// When `key` is boxed at `now_ns`, the time it is released;
    /// `None` when it is free to be decided by its limit.
    #[inline]
    pub fn check(&self, key: &[u8], now_ns: u64) -> Option<u64> {
        self.slot(key, |all, at, _| match at {
            Some(i) if all[i].until_ns > now_ns => Some(all[i].until_ns),
            _ => None,
        })
    }

    /// Records that `key`'s limit refused it at `now_ns`.
    pub fn refused(&self, key: &[u8], now_ns: u64) -> Strike {
        let cfg = self.cfg;
        let window = now_ns / cfg.window_ns;
        self.slot(key, |all, at, h| {
            let e = match at {
                Some(i) => &mut all[i],
                None => {
                    // A free slot near home, else the one released first.
                    let n = all.len();
                    let home = (h as usize) & (n - 1);
                    let idx = (0..PROBE)
                        .map(|i| (home + i) & (n - 1))
                        .min_by_key(|&i| (all[i].used, all[i].until_ns))
                        .expect("PROBE > 0");
                    all[idx] = Entry {
                        hash: h,
                        used: true,
                        ..Entry::default()
                    };
                    all[idx].last_window = u64::MAX;
                    &mut all[idx]
                }
            };
            if e.until_ns > now_ns {
                return Strike::Boxed {
                    until_ns: e.until_ns,
                    offence: e.offences,
                };
            }
            if e.offences > 0
                && e.until_ns != 0
                && now_ns.saturating_sub(e.until_ns) >= cfg.forget_ns
            {
                e.offences = 0;
            }
            e.streak = if e.last_window != u64::MAX && window == e.last_window {
                e.streak
            } else if e.last_window != u64::MAX && window == e.last_window + 1 {
                e.streak + 1
            } else {
                1
            };
            e.last_window = window;
            if e.streak < cfg.windows {
                return Strike::Counted { streak: e.streak };
            }
            e.offences += 1;
            let shift = (e.offences - 1).min(32);
            let timeout = cfg
                .timeout_ns
                .saturating_mul(1u64 << shift)
                .min(cfg.max_timeout_ns.max(cfg.timeout_ns));
            e.until_ns = now_ns.saturating_add(timeout);
            e.streak = 0;
            e.last_window = u64::MAX;
            Strike::Boxed {
                until_ns: e.until_ns,
                offence: e.offences,
            }
        })
    }

    /// Keys boxed at `now_ns`.
    pub fn boxed(&self, now_ns: u64) -> usize {
        self.shards
            .iter()
            .map(|s| {
                s.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .iter()
                    .filter(|e| e.used && e.until_ns > now_ns)
                    .count()
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000_000_000;

    fn cfg() -> PenaltyConfig {
        PenaltyConfig {
            windows: 3,
            window_ns: SEC,
            timeout_ns: 10 * SEC,
            max_timeout_ns: 35 * SEC,
            forget_ns: 60 * SEC,
            capacity: 4096,
        }
    }

    #[test]
    fn three_windows_in_a_row_box_the_key() {
        let p = PenaltyBox::new(cfg());
        let t = 100 * SEC;
        assert_eq!(p.check(b"k", t), None);
        assert_eq!(p.refused(b"k", t), Strike::Counted { streak: 1 });
        // Many refusals in one window are one window.
        assert_eq!(p.refused(b"k", t + 1), Strike::Counted { streak: 1 });
        assert_eq!(p.refused(b"k", t + SEC), Strike::Counted { streak: 2 });
        assert_eq!(
            p.refused(b"k", t + 2 * SEC),
            Strike::Boxed {
                until_ns: t + 12 * SEC,
                offence: 1
            }
        );
        assert_eq!(p.check(b"k", t + 2 * SEC), Some(t + 12 * SEC));
        assert_eq!(p.check(b"k", t + 12 * SEC - 1), Some(t + 12 * SEC));
        assert_eq!(p.check(b"k", t + 12 * SEC), None, "released on time");
        assert_eq!(p.check(b"other", t + 3 * SEC), None);
        assert_eq!(p.boxed(t + 3 * SEC), 1);
        // A gap breaks the streak.
        let q = PenaltyBox::new(cfg());
        q.refused(b"k", t);
        q.refused(b"k", t + SEC);
        assert_eq!(q.refused(b"k", t + 3 * SEC), Strike::Counted { streak: 1 });
    }

    #[test]
    fn repeat_offences_escalate_and_a_clean_spell_forgets() {
        let p = PenaltyBox::new(cfg());
        let mut t = 1000 * SEC;
        let box_once = |p: &PenaltyBox, t: u64| {
            p.refused(b"k", t);
            p.refused(b"k", t + SEC);
            p.refused(b"k", t + 2 * SEC)
        };
        let want = [10, 20, 35, 35];
        for (i, secs) in want.into_iter().enumerate() {
            let s = box_once(&p, t);
            assert_eq!(
                s,
                Strike::Boxed {
                    until_ns: t + 2 * SEC + secs * SEC,
                    offence: i as u32 + 1
                }
            );
            // Refusals while boxed change nothing.
            assert_eq!(p.refused(b"k", t + 3 * SEC), s);
            t += 2 * SEC + secs * SEC;
        }
        // Clean for a minute after release: back to the first timeout.
        let s = box_once(&p, t + 60 * SEC);
        assert!(matches!(s, Strike::Boxed { offence: 1, .. }), "{s:?}");
    }

    #[test]
    fn a_flood_of_keys_never_boxes_the_innocent() {
        let p = PenaltyBox::new(PenaltyConfig {
            capacity: 64,
            ..cfg()
        });
        let t = 50 * SEC;
        for w in 0..3 {
            p.refused(b"bad", t + w * SEC);
        }
        assert!(p.check(b"bad", t + 3 * SEC).is_some());
        for i in 0..100_000u32 {
            p.refused(&i.to_be_bytes(), t + 3 * SEC);
        }
        for i in 0..1000u32 {
            assert_eq!(p.check(&(i + 1_000_000).to_be_bytes(), t + 3 * SEC), None);
        }
        assert!(format!("{p:?}").contains("PenaltyBox"));
        assert_eq!(p.config().windows, 3);
    }
}
