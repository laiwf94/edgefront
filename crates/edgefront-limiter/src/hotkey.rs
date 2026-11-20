//! Hot keys: one popular key must not serialise every core on its one
//! bucket.
//!
//! A key in a [`Table`] is one entry behind one shard lock, so sixteen
//! threads deciding for the same key take turns on that lock and its
//! cache line. [`HotTable`] watches which keys are hot and moves each hot
//! key to a [`SplitBucket`]: one GCRA bucket per thread slot, each with
//! its share of the rate and the burst, so threads deciding for the hot
//! key work on cache lines of their own.
//!
//! - **Detection by sampled counts.** Each [`Handle`] (one per thread)
//!   samples one decision in `sample_every` into a shared space-saving
//!   counter of 64 slots. Every `window` samples, a key holding at least
//!   `threshold_pct` of them is hot, a hot key below half that is cooled,
//!   and the counts halve, so the set follows the traffic.
//! - **Sub-buckets.** A split of `n` sub-buckets gives each the rate's
//!   tokens per `n` periods and a share of the burst, the shares summing
//!   to the burst, so the sum of the parts is the rate. A decision takes
//!   from the thread's own sub-bucket, then once from the next; a cost
//!   larger than a share is gathered across all of them, all or nothing.
//! - **Periodic rebalance.** Traffic is rarely even across threads, and
//!   a sub-bucket whose thread is busier runs dry while others fill. Every
//!   `rebalance_ns` of clock the first thread past the mark moves tokens
//!   from the fullest sub-buckets to the emptiest, so each holds its share
//!   of what is left; the total is kept exactly. Between rebalances the
//!   split can lag an exact bucket by under one token per sub-bucket.
//! - **Moving between the two.** A key made hot starts its split with the
//!   tokens its table entry had; a key cooled goes back to its entry with
//!   the split's remaining tokens. Decisions the table took while the
//!   split was being made are charged to the split once it is published,
//!   so only a decision already past its hot-set check at that moment (at
//!   most one per thread) is not; the hot set is published through
//!   [`crate::cidr::Swap`], so a decision reads it with one atomic load.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::cidr::{Reader, Swap};
use crate::gcra::{Decision, Error, Gcra, Rate};
use crate::table::Table;

/// A bucket on its own cache lines.
#[derive(Debug)]
#[repr(align(128))]
struct Padded(Gcra);

/// One key's bucket split into per-thread sub-buckets.
#[derive(Debug)]
pub struct SplitBucket {
    subs: Box<[Padded]>,
    shares: Box<[u64]>,
    next_rebalance: AtomicU64,
    rebalance_ns: u64,
}

impl SplitBucket {
    /// `rate` split `n` ways (fewer when the burst has fewer tokens than
    /// `n`), every sub-bucket full, rebalanced every `rebalance_ns`.
    pub fn new(rate: &Rate, n: usize, rebalance_ns: u64) -> Result<Self, Error> {
        let n = n.clamp(1, usize::try_from(rate.burst.max(1)).unwrap_or(usize::MAX)) as u64;
        let mut subs = Vec::with_capacity(n as usize);
        let mut shares = Vec::with_capacity(n as usize);
        for i in 0..n {
            let share = rate.burst / n + u64::from(i < rate.burst % n);
            let period = rate.period_ns.checked_mul(n).ok_or(Error::InvalidRate)?;
            subs.push(Padded(Gcra::new(&Rate::new(rate.tokens, period, share))?));
            shares.push(share);
        }
        Ok(SplitBucket {
            subs: subs.into(),
            shares: shares.into(),
            next_rebalance: AtomicU64::new(0),
            rebalance_ns: rebalance_ns.max(1),
        })
    }

    /// How many sub-buckets there are.
    pub fn parts(&self) -> usize {
        self.subs.len()
    }

    /// Tokens left across every sub-bucket.
    pub fn remaining(&self, now_ns: u64) -> u64 {
        self.subs.iter().map(|s| s.0.remaining(now_ns)).sum()
    }

    /// Takes `cost` for the thread in slot `home`.
    pub fn take(&self, home: usize, now_ns: u64, cost: u64) -> Decision {
        self.maybe_rebalance(now_ns);
        let n = self.subs.len();
        let own = home % n;
        if cost <= self.shares[own] {
            match self.subs[own].0.take(now_ns, cost) {
                Ok(d) if d.allowed => return d,
                Ok(d) => {
                    let next = (own + 1) % n;
                    if next != own
                        && let Ok(d2) = self.subs[next].0.take(now_ns, cost)
                        && d2.allowed
                    {
                        return d2;
                    }
                    return d;
                }
                Err(_) => {}
            }
        }
        self.gather(own, now_ns, cost)
    }

    /// A cost bigger than one share, taken across every sub-bucket.
    fn gather(&self, own: usize, now_ns: u64, cost: u64) -> Decision {
        let total: u64 = self.shares.iter().sum();
        if cost > total {
            return Decision::default();
        }
        let n = self.subs.len();
        let mut got = Vec::with_capacity(n);
        let mut need = cost;
        for i in 0..n {
            if need == 0 {
                break;
            }
            let s = &self.subs[(own + i) % n].0;
            let d = s.take_upto(now_ns, need);
            need -= d.granted;
            got.push(((own + i) % n, d.granted));
        }
        if need > 0 {
            for (i, g) in got {
                self.subs[i].0.refund(now_ns, g);
            }
            // When the shortfall comes back at the whole rate.
            let iv = self.subs[0].0.interval() / n as u64;
            let wait = (u128::from(need) * u128::from(iv)) >> crate::gcra::TICK_SHIFT;
            return Decision {
                allowed: false,
                granted: 0,
                remaining: 0,
                retry_after_ns: u64::try_from(wait).unwrap_or(u64::MAX).max(1),
            };
        }
        Decision {
            allowed: true,
            granted: cost,
            remaining: self.remaining(now_ns),
            retry_after_ns: 0,
        }
    }

    fn maybe_rebalance(&self, now_ns: u64) {
        let due = self.next_rebalance.load(Ordering::Relaxed);
        if now_ns < due
            || self
                .next_rebalance
                .compare_exchange(
                    due,
                    now_ns.saturating_add(self.rebalance_ns),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_err()
        {
            return;
        }
        self.rebalance(now_ns);
    }

    /// Spreads the remaining tokens over the sub-buckets by their
    /// shares, moving tokens from those above their part to those below.
    pub fn rebalance(&self, now_ns: u64) {
        let have: Vec<u64> = self.subs.iter().map(|s| s.0.remaining(now_ns)).collect();
        let total: u64 = have.iter().sum();
        let burst: u64 = self.shares.iter().sum();
        let mut moved = 0u64;
        // Take the surplus first, then hand it to those short of their part.
        for (i, s) in self.subs.iter().enumerate() {
            let want = total * self.shares[i] / burst;
            if have[i] > want {
                moved += s.0.take_upto(now_ns, have[i] - want).granted;
            }
        }
        for (i, s) in self.subs.iter().enumerate() {
            let want = total * self.shares[i] / burst;
            if have[i] < want && moved > 0 {
                let give = (want - have[i]).min(moved);
                s.0.refund(now_ns, give);
                moved -= give;
            }
        }
        // Rounding leftovers go one at a time to those with room.
        while moved > 0 {
            let before = moved;
            for sub in self.subs.iter() {
                if moved == 0 {
                    break;
                }
                let g = &sub.0;
                let cap = g.tolerance() / g.interval();
                if g.remaining(now_ns) < cap {
                    g.refund(now_ns, 1);
                    moved -= 1;
                }
            }
            if moved == before {
                break;
            }
        }
    }
}

/// When keys count as hot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotConfig {
    /// A power of two: each handle samples one decision in this many.
    pub sample_every: u32,
    /// Samples between decisions about the hot set.
    pub window: u32,
    /// The share of samples, in percent, that makes a key hot.
    pub threshold_pct: u32,
    /// The most keys hot at once.
    pub max_hot: usize,
    /// Sub-buckets per hot key: usually the worker threads.
    pub parts: usize,
    /// How often a split is rebalanced.
    pub rebalance_ns: u64,
}

impl Default for HotConfig {
    fn default() -> Self {
        HotConfig {
            sample_every: 256,
            window: 2048,
            threshold_pct: 5,
            max_hot: 16,
            parts: 16,
            rebalance_ns: 1_000_000,
        }
    }
}

type HotSet = Vec<(Box<[u8]>, Arc<SplitBucket>)>;

/// Space-saving counts of sampled keys.
#[derive(Default)]
struct Sampler {
    slots: Vec<(Vec<u8>, u64)>,
    total: u64,
}

/// A [`Table`] that moves its hot keys to split buckets.
pub struct HotTable {
    table: Table,
    rate: Rate,
    cfg: HotConfig,
    hot: Arc<Swap<HotSet>>,
    sampler: Mutex<Sampler>,
    promotions: AtomicU64,
}

impl std::fmt::Debug for HotTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HotTable")
            .field("hot", &self.hot.load().len())
            .finish()
    }
}

const SLOTS: usize = 64;

impl HotTable {
    /// `table` with its rate `rate` (the table's own), watched as `cfg`.
    pub fn new(table: Table, rate: Rate, cfg: HotConfig) -> Arc<Self> {
        let cfg = HotConfig {
            sample_every: cfg.sample_every.max(1).next_power_of_two(),
            window: cfg.window.max(1),
            ..cfg
        };
        Arc::new(HotTable {
            table,
            rate,
            cfg,
            hot: Arc::new(Swap::new(Vec::new())),
            sampler: Mutex::new(Sampler::default()),
            promotions: AtomicU64::new(0),
        })
    }

    /// A handle for the thread in slot `home`.
    pub fn handle(self: &Arc<Self>, home: usize) -> Handle {
        Handle {
            table: Arc::clone(self),
            reader: self.hot.reader(),
            home,
            tick: 0,
        }
    }

    /// The keys hot now.
    pub fn hot_keys(&self) -> Vec<Vec<u8>> {
        self.hot.load().iter().map(|(k, _)| k.to_vec()).collect()
    }

    /// How many times a key has been made hot.
    pub fn promotions(&self) -> u64 {
        self.promotions.load(Ordering::Relaxed)
    }

    pub fn table(&self) -> &Table {
        &self.table
    }

    fn sample(&self, key: &[u8], now_ns: u64) {
        let mut s = self.sampler.lock().unwrap_or_else(PoisonError::into_inner);
        s.total += 1;
        if let Some(slot) = s.slots.iter_mut().find(|(k, _)| k == key) {
            slot.1 += 1;
        } else if s.slots.len() < SLOTS {
            s.slots.push((key.to_vec(), 1));
        } else if let Some(min) = s.slots.iter_mut().min_by_key(|(_, c)| *c) {
            // Space-saving: the newcomer inherits the smallest count.
            *min = (key.to_vec(), min.1 + 1);
        }
        if s.total < u64::from(self.cfg.window) {
            return;
        }
        let total = s.total;
        let pct = u64::from(self.cfg.threshold_pct);
        let current = self.hot.load();
        let mut next: HotSet = Vec::with_capacity(self.cfg.max_hot);
        // Keep hot keys still above half the threshold.
        for (k, b) in current.iter() {
            let count = s
                .slots
                .iter()
                .find(|(sk, _)| **sk == **k)
                .map_or(0, |x| x.1);
            if count * 100 * 2 >= pct * total {
                next.push((k.clone(), Arc::clone(b)));
            } else {
                self.cool(k, b, now_ns);
            }
        }
        let mut candidates: Vec<&(Vec<u8>, u64)> = s
            .slots
            .iter()
            .filter(|(k, c)| c * 100 >= pct * total && !next.iter().any(|(h, _)| **h == **k))
            .collect();
        candidates.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
        let mut heated = Vec::new();
        for (k, _) in candidates {
            if next.len() >= self.cfg.max_hot {
                break;
            }
            if let Some((b, left)) = self.heat(k, now_ns) {
                let b = Arc::new(b);
                heated.push((k.clone(), left, Arc::clone(&b)));
                next.push((k.clone().into_boxed_slice(), b));
                self.promotions.fetch_add(1, Ordering::Relaxed);
            }
        }
        for slot in &mut s.slots {
            slot.1 /= 2;
        }
        s.slots.retain(|(_, c)| *c > 0);
        s.total /= 2;
        self.hot.store(next);
        // Decisions that reached the table between reading its entry and
        // publishing the split are charged to the split now; only those
        // already past the hot-set check when it was published are not.
        for (k, left, b) in heated {
            let now_left = self.table.remaining(&k, now_ns).unwrap_or(self.rate.burst);
            let mut spend = left.saturating_sub(now_left);
            for sub in b.subs.iter() {
                if spend == 0 {
                    break;
                }
                spend -= sub.0.take_upto(now_ns, spend).granted;
            }
        }
    }

    /// A split for `key` holding what its table entry had.
    fn heat(&self, key: &[u8], now_ns: u64) -> Option<(SplitBucket, u64)> {
        let b = SplitBucket::new(&self.rate, self.cfg.parts, self.cfg.rebalance_ns).ok()?;
        let left = self.table.remaining(key, now_ns).unwrap_or(self.rate.burst);
        let mut spend = self.rate.burst.saturating_sub(left);
        for s in b.subs.iter() {
            if spend == 0 {
                break;
            }
            spend -= s.0.take_upto(now_ns, spend).granted;
        }
        Some((b, left))
    }

    /// Back to the table entry, with the split's tokens.
    fn cool(&self, key: &[u8], b: &SplitBucket, now_ns: u64) {
        let left = b.remaining(now_ns).min(self.rate.burst);
        let _ = self.table.remove(key);
        let spent = self.rate.burst - left;
        if spent > 0 {
            let _ = self.table.take(key, now_ns, spent);
        }
    }
}

/// One thread's way into a [`HotTable`].
pub struct Handle {
    table: Arc<HotTable>,
    reader: Reader<HotSet>,
    home: usize,
    tick: u32,
}

impl Handle {
    /// [`Table::take`] for `key`, from its split when it is hot.
    pub fn take(&mut self, key: &[u8], now_ns: u64, cost: u64) -> Result<Decision, Error> {
        self.tick = self.tick.wrapping_add(1);
        if self.tick & (self.table.cfg.sample_every - 1) == 0 {
            self.table.sample(key, now_ns);
        }
        if let Some((_, b)) = self.reader.get().iter().find(|(k, _)| **k == *key) {
            return Ok(b.take(self.home, now_ns, cost));
        }
        self.table.table.take(key, now_ns, cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::TableConfig;

    const SEC: u64 = 1_000_000_000;

    #[test]
    fn the_parts_sum_to_the_whole() {
        let rate = Rate::new(1000, SEC, 100);
        let b = SplitBucket::new(&rate, 16, SEC).unwrap();
        assert_eq!(b.parts(), 16);
        assert_eq!(b.shares.iter().sum::<u64>(), 100);
        assert_eq!(b.remaining(0), 100);
        // A burst smaller than the parts: one part per token.
        assert_eq!(
            SplitBucket::new(&Rate::new(1, SEC, 3), 16, SEC)
                .unwrap()
                .parts(),
            3
        );
        // A cost larger than a share is gathered, all or nothing.
        let d = b.take(0, 0, 50);
        assert!(d.allowed);
        assert_eq!(b.remaining(0), 50);
        assert!(!b.take(3, 0, 51).allowed);
        assert_eq!(b.remaining(0), 50, "a refused gather refunds");
        assert!(!b.take(3, 0, 101).allowed);
        assert!(b.take(3, 0, 50).allowed);
        let d = b.take(3, 0, 50);
        assert!(!d.allowed && d.retry_after_ns > 0);
    }

    #[test]
    fn rebalancing_keeps_the_total_and_evens_the_parts() {
        let rate = Rate::new(1, 1000 * SEC, 160);
        let b = SplitBucket::new(&rate, 16, u64::MAX).unwrap();
        // Thread 0 drains its part and its neighbour's.
        while b.take(0, 1, 1).allowed {}
        let before = b.remaining(1);
        assert_eq!(before, 160 - 20);
        b.rebalance(1);
        assert_eq!(b.remaining(1), before);
        let parts: Vec<u64> = b.subs.iter().map(|s| s.0.remaining(1)).collect();
        assert!(parts.iter().all(|&p| (8..=9).contains(&p)), "{parts:?}");
        // Thread 0 can take again.
        assert!(b.take(0, 1, 1).allowed);
    }

    /// One key at 10,000 a second with a burst of 1,600, offered twice
    /// that from 16 threads with uneven loads (thread t offers t + 1
    /// shares), over 10 s of simulated time: the split admits within 1%
    /// of what an exact bucket admits.
    #[test]
    fn a_split_is_within_one_percent_of_an_exact_bucket() {
        let rate = Rate::new(10_000, SEC, 1600);
        let exact = Gcra::new(&rate).unwrap();
        let split = SplitBucket::new(&rate, 16, SEC / 1000).unwrap();
        let (mut e, mut s) = (0u64, 0u64);
        let weights: u64 = (1..=16).sum();
        let mut now = SEC;
        let end = now + 10 * SEC;
        let mut t = 0u64;
        while now < end {
            // 20,000 a second, thread chosen by weight.
            t = (t + 1) % weights;
            let mut acc = 0;
            let home = (1..=16u64)
                .position(|w| {
                    acc += w;
                    t < acc
                })
                .unwrap();
            e += u64::from(exact.take(now, 1).unwrap().allowed);
            s += u64::from(split.take(home, now, 1).allowed);
            now += SEC / 20_000;
        }
        let err = (s as f64 - e as f64).abs() / e as f64;
        assert!(err < 0.01, "split {s}, exact {e}: {:.3}%", err * 100.0);
    }

    #[test]
    fn hot_keys_move_to_splits_and_back() {
        let rate = Rate::new(100, SEC, 100);
        let table = Table::new(&TableConfig::new(1 << 14, rate)).unwrap();
        let cfg = HotConfig {
            sample_every: 1,
            window: 100,
            threshold_pct: 20,
            max_hot: 4,
            parts: 4,
            rebalance_ns: SEC,
        };
        let ht = HotTable::new(table, rate, cfg);
        let mut h = ht.handle(0);
        let now = 10 * SEC;
        // Spend 30 of the hot key's tokens in the table first.
        for _ in 0..30 {
            assert!(h.take(b"hot", now, 1).unwrap().allowed);
        }
        // Then 9 in 10 decisions for it, the rest spread wide.
        for i in 0..400u32 {
            if i % 10 == 0 {
                h.take(&i.to_be_bytes(), now, 1).unwrap();
            } else {
                h.take(b"hot", now, 1).unwrap();
            }
        }
        assert_eq!(ht.hot_keys(), [b"hot".to_vec()]);
        assert_eq!(ht.promotions(), 1);
        // It kept its debt: 100 tokens, all spent by now.
        assert!(!h.take(b"hot", now, 1).unwrap().allowed);
        // Cold for a while: it goes back to the table with what it has.
        let later = now + SEC;
        for i in 0..1000u32 {
            h.take(&(i % 50).to_be_bytes(), later, 1).unwrap();
        }
        assert!(ht.hot_keys().is_empty());
        // A full bucket needs no entry.
        assert_eq!(ht.table().remaining(b"hot", later).unwrap_or(100), 100);
        assert!(format!("{ht:?}").contains("hot: 0"));
    }

    /// 16 threads on one hot key take no more than the exact bucket
    /// allows: the rate's tokens over the run plus the burst.
    #[test]
    fn sixteen_threads_on_a_hot_key_stay_within_the_rate() {
        let rate = Rate::new(1_000_000, SEC, 1000);
        let table = Table::new(&TableConfig::new(1 << 14, rate)).unwrap();
        let ht = HotTable::new(table, rate, HotConfig::default());
        let clock = AtomicU64::new(SEC);
        let admitted = AtomicU64::new(0);
        std::thread::scope(|s| {
            for t in 0..16 {
                let (ht, clock, admitted) = (&ht, &clock, &admitted);
                s.spawn(move || {
                    let mut h = ht.handle(t);
                    for _ in 0..200_000 {
                        let now = clock.fetch_add(25, Ordering::Relaxed);
                        if h.take(b"hot", now, 1).unwrap().allowed {
                            admitted.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        let elapsed = clock.load(Ordering::Relaxed) - SEC;
        // At most one decision per thread can race the move to the split.
        let most = 1000 + elapsed / 1000 + 1 + 16;
        let got = admitted.load(Ordering::Relaxed);
        assert!(got <= most, "{got} > {most}");
        // A thread descheduled between reading the clock and deciding
        // decides on a stale time, which only ever refuses, so on a busy
        // machine the count falls a little short.
        assert!(got as f64 > most as f64 * 0.9, "{got} of {most}");
        assert_eq!(ht.hot_keys(), [b"hot".to_vec()]);
    }
}
