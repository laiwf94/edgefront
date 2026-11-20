//! All-or-nothing decisions across several limits: a request checked
//! against a per-client, a per-route and a global limit either takes
//! from all of them or from none.
//!
//! [`take_all`] takes from each limit in the order given, each with one
//! lock-free [`Gcra::take`] or one shard-locked [`Table::take`] (no lock
//! is held across limits, so nothing can deadlock and a slow limit never
//! holds up another's callers). The first that refuses stops the walk,
//! and every token taken before it is handed back with `refund`, newest
//! first. A refused request therefore leaves every bucket where it would
//! have been had the request never come, and an admitted one has taken
//! its cost from each.
//!
//! What another thread can see in between is the attempt: for the moment
//! between a take and its refund, a bucket holds a token that will be
//! given back, so a concurrent request on that bucket alone may be
//! refused where it would have been admitted a moment later. That errs
//! towards refusing, never towards admitting more than every limit
//! allows, and it is what the order is for: put the limit most likely to
//! refuse first, so refused requests touch as few buckets as possible.
//!
//! Refunds are exact while the bucket's debt covers them, which it always
//! does here: the refund is at the same `now_ns` as the take, and a take
//! of `cost` added exactly `cost` intervals of debt (see
//! [`Gcra::refund`]). [`MAX_LIMITS`] bounds the walk so the record of
//! what was taken stays on the stack.

use crate::gcra::{Decision, Error, Gcra};
use crate::table::Table;

/// The most limits one decision may cover.
pub const MAX_LIMITS: usize = 16;

/// One limit of a decision, with the request's cost in it.
#[derive(Clone, Copy, Debug)]
pub enum Limit<'a> {
    /// A shared bucket.
    Shared { gcra: &'a Gcra, cost: u64 },
    /// A key's bucket in a table.
    Keyed {
        table: &'a Table,
        key: &'a [u8],
        cost: u64,
    },
}

impl Limit<'_> {
    fn take(&self, now_ns: u64) -> Result<Decision, Error> {
        match *self {
            Limit::Shared { gcra, cost } => gcra.take(now_ns, cost),
            Limit::Keyed { table, key, cost } => table.take(key, now_ns, cost),
        }
    }

    fn refund(&self, now_ns: u64) {
        match *self {
            Limit::Shared { gcra, cost } => gcra.refund(now_ns, cost),
            Limit::Keyed { table, key, cost } => table.refund(key, now_ns, cost),
        }
    }
}

/// The outcome of [`take_all`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MultiDecision {
    /// Every limit admitted the request. `remaining` is the fewest tokens
    /// any limit has left.
    Admitted { remaining: u64 },
    /// Limit `index` refused it, and every earlier take was refunded.
    /// `retry_after_ns` is that limit's wait for the same cost.
    Refused { index: usize, retry_after_ns: u64 },
    /// Limit `index` could not decide (a cost above its burst, a full
    /// table, a bad key); every earlier take was refunded.
    Failed { index: usize, error: Error },
}

impl MultiDecision {
    pub fn admitted(&self) -> bool {
        matches!(self, MultiDecision::Admitted { .. })
    }
}

/// Takes from every limit in `limits`, in order, all or nothing. More
/// than [`MAX_LIMITS`] limits is refused with [`Error::InvalidRate`]
/// naming index [`MAX_LIMITS`], before anything is taken.
pub fn take_all(limits: &[Limit<'_>], now_ns: u64) -> MultiDecision {
    if limits.len() > MAX_LIMITS {
        return MultiDecision::Failed {
            index: MAX_LIMITS,
            error: Error::InvalidRate,
        };
    }
    let mut remaining = u64::MAX;
    for (i, limit) in limits.iter().enumerate() {
        let out = match limit.take(now_ns) {
            Ok(d) if d.allowed => {
                remaining = remaining.min(d.remaining);
                continue;
            }
            Ok(d) => MultiDecision::Refused {
                index: i,
                retry_after_ns: d.retry_after_ns,
            },
            Err(error) => MultiDecision::Failed { index: i, error },
        };
        for earlier in limits[..i].iter().rev() {
            earlier.refund(now_ns);
        }
        return out;
    }
    MultiDecision::Admitted {
        remaining: if limits.is_empty() { 0 } else { remaining },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use proptest::prelude::*;

    use super::*;
    use crate::gcra::Rate;
    use crate::table::TableConfig;

    const SEC: u64 = 1_000_000_000;

    #[test]
    fn a_refusal_spends_nothing_and_an_admission_spends_everywhere() {
        let a = Gcra::new(&Rate::new(1, SEC, 10)).unwrap();
        let b = Gcra::new(&Rate::new(1, SEC, 3)).unwrap();
        let t = Table::new(&TableConfig::new(16, Rate::new(1, SEC, 5))).unwrap();
        let now = 10 * SEC;
        let limits = [
            Limit::Shared { gcra: &a, cost: 1 },
            Limit::Keyed {
                table: &t,
                key: b"k",
                cost: 1,
            },
            Limit::Shared { gcra: &b, cost: 1 },
        ];
        for left in [2, 1, 0] {
            assert_eq!(
                take_all(&limits, now),
                MultiDecision::Admitted { remaining: left }
            );
        }
        let refused = take_all(&limits, now);
        assert_eq!(
            refused,
            MultiDecision::Refused {
                index: 2,
                retry_after_ns: SEC
            }
        );
        assert!(!refused.admitted());
        // The refusals took nothing from the first two.
        for _ in 0..20 {
            take_all(&limits, now);
        }
        assert_eq!(a.remaining(now), 7);
        assert_eq!(t.remaining(b"k", now), Some(2));
        assert_eq!(b.remaining(now), 0);
        // A failure refunds too.
        let big = [
            Limit::Shared { gcra: &a, cost: 1 },
            Limit::Shared { gcra: &b, cost: 9 },
        ];
        assert_eq!(
            take_all(&big, now),
            MultiDecision::Failed {
                index: 1,
                error: Error::CostExceedsBurst
            }
        );
        assert_eq!(a.remaining(now), 7);
        assert_eq!(take_all(&[], now), MultiDecision::Admitted { remaining: 0 });
        let many = [Limit::Shared { gcra: &a, cost: 1 }; 17];
        assert_eq!(
            take_all(&many, now),
            MultiDecision::Failed {
                index: 16,
                error: Error::InvalidRate
            }
        );
        assert_eq!(a.remaining(now), 7, "nothing taken");
    }

    /// The issue's proof: requests checked against five limits each, from
    /// 16 threads at once, on a clock that does not move so no bucket
    /// refills. At the end every bucket has given out exactly one token
    /// per admitted request that named it: no refused request kept a
    /// token anywhere, and no admitted one skipped a limit.
    fn concurrent(seed: u64, bursts: [u64; 8], per_thread: usize) {
        let now = 100 * SEC;
        let shared: Vec<Gcra> = bursts
            .iter()
            .map(|&b| Gcra::new(&Rate::new(1, SEC, b)).unwrap())
            .collect();
        let table = Table::new(&TableConfig::new(64, Rate::new(1, SEC, 40))).unwrap();
        let keys: [&[u8]; 4] = [b"a", b"b", b"c", b"d"];
        let given = Arc::new([const { AtomicU64::new(0) }; 12]);
        std::thread::scope(|s| {
            for t in 0..16u64 {
                let shared = &shared;
                let table = &table;
                let given = Arc::clone(&given);
                s.spawn(move || {
                    let mut x = seed ^ (t + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
                    let mut next = || {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        x
                    };
                    for _ in 0..per_thread {
                        // Four distinct shared limits and one keyed.
                        let mut picks = Vec::with_capacity(5);
                        while picks.len() < 4 {
                            let p = (next() % 8) as usize;
                            if !picks.contains(&p) {
                                picks.push(p);
                            }
                        }
                        let k = (next() % 4) as usize;
                        let mut limits: Vec<Limit> = picks
                            .iter()
                            .map(|&p| Limit::Shared {
                                gcra: &shared[p],
                                cost: 1,
                            })
                            .collect();
                        limits.push(Limit::Keyed {
                            table,
                            key: keys[k],
                            cost: 1,
                        });
                        if take_all(&limits, now).admitted() {
                            for &p in &picks {
                                given[p].fetch_add(1, Ordering::Relaxed);
                            }
                            given[8 + k].fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        for (i, g) in shared.iter().enumerate() {
            let spent = bursts[i] - g.remaining(now);
            assert_eq!(spent, given[i].load(Ordering::Relaxed), "shared limit {i}");
        }
        for (k, key) in keys.iter().enumerate() {
            let spent = 40 - table.remaining(key, now).unwrap_or(40);
            assert_eq!(spent, given[8 + k].load(Ordering::Relaxed), "key {k}");
        }
    }

    #[test]
    fn sixteen_threads_take_from_all_or_none() {
        concurrent(1, [5, 50, 200, 1000, 10, 3000, 70, 400], 2000);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]
        #[test]
        fn all_or_nothing_holds_under_contention(
            seed in any::<u64>(),
            bursts in proptest::array::uniform8(1u64..500),
        ) {
            concurrent(seed, bursts, 300);
        }
    }
}
