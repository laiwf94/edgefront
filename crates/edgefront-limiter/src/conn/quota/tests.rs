use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::AtomicUsize;
use std::sync::{Barrier, Mutex};
use std::thread;

use super::*;
use crate::rate::Quota;

const SEC: u64 = 1_000_000_000;
const HOUR: u64 = 3_600 * SEC;
const DAY: u64 = 86_400 * SEC;
/// A bucket of a one-hour window: 225 s.
const SLOT: u64 = HOUR / QUOTA_SLOTS as u64;

fn key(i: u32) -> ClientKey {
    ClientKey::new(IpAddr::V4(Ipv4Addr::from(i)), 64)
}

fn table(size: u32, limit: u64, window_ns: u64, policy: FullPolicy) -> QuotaTable {
    QuotaTable::new(size, limit, window_ns, policy).unwrap()
}

/// splitmix64, as the crate's other seeded tests use.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn new_refuses_what_a_quota_refuses() {
    let p = FullPolicy::Deny;
    assert_eq!(
        QuotaTable::new(0, 1, HOUR, p).unwrap_err(),
        Error::InvalidConfig
    );
    assert_eq!(
        QuotaTable::new(MAX_ENTRIES + 1, 1, HOUR, p).unwrap_err(),
        Error::InvalidConfig
    );
    assert_eq!(
        QuotaTable::new(4, 0, HOUR, p).unwrap_err(),
        Error::InvalidRate
    );
    assert_eq!(
        QuotaTable::new(4, 1, 15, p).unwrap_err(),
        Error::InvalidWindow
    );
    assert!(QuotaTable::new(4, 1, 16, p).is_ok());
    let t = table(4, 3, HOUR, p);
    assert_eq!(t.set_limit(0), Err(Error::InvalidRate));
    assert_eq!(t.reset(15), Err(Error::InvalidWindow));
    assert_eq!((t.limit(), t.window_ns()), (3, HOUR), "nothing changed");
}

/// The window edge, worked by hand for 3 per hour in buckets of 225 s.
#[test]
fn the_window_edge() {
    let t = table(16, 3, HOUR, FullPolicy::Deny);
    let k = key(1);
    // Three at 100 s, in the bucket [0, 225 s).
    for _ in 0..3 {
        assert_eq!(t.take(&k, 100 * SEC), Ok(true));
    }
    assert_eq!(t.take(&k, 100 * SEC), Ok(false), "the fourth is refused");
    assert_eq!(t.count(&k, 100 * SEC), Some(3), "a refusal counts nothing");
    // Up to an hour after the bucket started, all three are in the window.
    assert_eq!(t.take(&k, HOUR - 1), Ok(false));
    assert_eq!(t.take(&k, HOUR), Ok(false));
    // From then on the oldest bucket is weighted by the part of it still
    // inside the window: 3 * (225 s - 1 ns) / 225 s rounds down to 2.
    assert_eq!(t.count(&k, HOUR + 1), Some(2));
    assert_eq!(t.take(&k, HOUR + 1), Ok(true));
    assert_eq!(t.take(&k, HOUR + 1), Ok(false));
    // A third of the way into the bucket, one more of the three has gone.
    assert_eq!(t.count(&k, HOUR + SLOT / 3 + 1), Some(2));
    assert_eq!(t.take(&k, HOUR + SLOT / 3 + 1), Ok(true));
    // Once the bucket has gone, only the two taken since are counted.
    assert_eq!(t.count(&k, HOUR + SLOT), Some(2));
    // Other clients have their own windows.
    assert_eq!(t.take(&key(2), HOUR), Ok(true));
    assert_eq!(t.count(&key(3), HOUR), None);
    t.check_invariants().unwrap();
}

/// Windows start on a multiple of the bucket width, so two clients seen
/// at different times in one bucket lose their counts at the same time.
#[test]
fn windows_start_on_the_bucket_grid() {
    let t = table(16, 1, HOUR, FullPolicy::Deny);
    assert_eq!(t.take(&key(1), 10 * SEC), Ok(true));
    assert_eq!(t.take(&key(2), SLOT - 1), Ok(true));
    for k in [key(1), key(2)] {
        assert_eq!(t.take(&k, HOUR), Ok(false));
        assert_eq!(t.take(&k, HOUR + SLOT), Ok(true));
    }
}

/// Every client's decisions are those of a `Quota` of its own whose window
/// starts on the bucket grid, through reclaims and rejections: 300 seeded
/// scripts over keys that outnumber a small table, with time moving in
/// steps from nanoseconds to whole windows.
#[test]
fn decisions_match_a_quota_per_client() {
    for seed in 0..300u64 {
        let mut rng = Rng(seed);
        let limit = 1 + rng.below(6);
        let window = [HOUR, 6 * HOUR, DAY, 16 * 31][rng.below(4) as usize];
        let slot = window / u64::from(QUOTA_SLOTS);
        let size = 1 + rng.below(12) as u32;
        let keys = 1 + rng.below(20) as u32;
        let t = table(size, limit, window, FullPolicy::Deny);
        let mut model: HashMap<u32, Quota> = HashMap::new();
        let mut now = rng.below(1 << 40);
        for step in 0..1_000 {
            now += match rng.below(4) {
                0 => 0,
                1 => rng.below(slot),
                2 => rng.below(window),
                _ => rng.below(3 * window),
            };
            let k = rng.below(u64::from(keys)) as u32;
            let got = t.take(&key(k), now);
            let ctx = format!("seed {seed} step {step} key {k} at {now}");
            match got {
                Ok(admitted) => {
                    let q = model
                        .entry(k)
                        .or_insert_with(|| Quota::new(limit, window, now - now % slot).unwrap());
                    assert_eq!(admitted, q.take(now, 1).unwrap().allowed, "{ctx}");
                    assert_eq!(t.count(&key(k), now), Some(window_count(q, now)), "{ctx}");
                }
                Err(e) => {
                    assert_eq!(e, Error::TableFull, "{ctx}");
                    assert_eq!(t.count(&key(k), now), None, "{ctx}");
                    // A client refused for a full table was not tracked, or
                    // its entry was reclaimed when idle: either way its
                    // window is empty.
                    if let Some(q) = model.get_mut(&k) {
                        assert_eq!(window_count(q, now), 0, "{ctx}");
                    }
                }
            }
            if step % 100 == 0 {
                t.check_invariants()
                    .map_err(|e| format!("{ctx}: {e}"))
                    .unwrap();
            }
        }
        let s = t.stats();
        assert!(s.entries <= u64::from(size), "seed {seed}");
        assert_eq!(
            s.inserts - s.reclaims - s.removals,
            s.entries,
            "seed {seed}"
        );
        t.check_invariants().unwrap();
    }
}

fn window_count(q: &mut Quota, now: u64) -> u64 {
    q.limit() - q.remaining(now)
}

#[test]
fn a_full_table_refuses_new_clients_until_one_is_idle() {
    let t = table(4, 2, HOUR, FullPolicy::Deny);
    for i in 1..=4 {
        assert_eq!(t.take(&key(i), 0), Ok(true));
    }
    assert_eq!(t.take(&key(5), 0), Err(Error::TableFull));
    assert_eq!(
        t.take(&key(5), HOUR),
        Err(Error::TableFull),
        "every window still counts"
    );
    // Existing clients keep their counts.
    assert_eq!(t.take(&key(1), 0), Ok(true));
    assert_eq!(t.take(&key(1), 0), Ok(false));
    let s = t.stats();
    assert_eq!(
        (s.entries, s.capacity, s.rejections, s.reclaims),
        (4, 4, 2, 0)
    );
    // Past the window and its oldest bucket, the coldest client (2) is
    // idle and is reclaimed for the new one.
    assert_eq!(t.take(&key(5), HOUR + SLOT), Ok(true));
    assert_eq!(t.count(&key(2), HOUR + SLOT), None);
    assert_eq!(t.stats().reclaims, 1);
    t.check_invariants().unwrap();
}

#[test]
fn evict_drops_the_least_recently_used_with_its_count() {
    let t = table(4, 1, DAY, FullPolicy::Evict);
    for i in 1..=4 {
        assert_eq!(t.take(&key(i), 0), Ok(true));
    }
    // Touching 1 makes 2 the coldest; a refusal is a use too.
    assert_eq!(t.take(&key(1), 0), Ok(false));
    assert_eq!(t.take(&key(5), 0), Ok(true));
    assert_eq!(t.count(&key(2), 0), None);
    assert_eq!(t.stats().evictions, 1);
    // The evicted client starts again with an empty window.
    assert_eq!(t.take(&key(2), 0), Ok(true));
    assert_eq!(t.stats().evictions, 2);
    t.check_invariants().unwrap();
}

#[test]
fn a_clock_that_goes_back_keeps_the_window() {
    let t = table(1, 1, HOUR, FullPolicy::Deny);
    assert_eq!(t.take(&key(1), 10 * HOUR), Ok(true));
    // An earlier time counts as the newest seen.
    assert_eq!(t.take(&key(1), HOUR), Ok(false));
    assert_eq!(t.take(&key(2), 9 * HOUR), Err(Error::TableFull));
    // Its window is empty at 12 h but starts after 2 h, so it is not idle
    // then, and a fresh window at 2 h would differ.
    assert_eq!(t.count(&key(1), 12 * HOUR), Some(0));
    assert_eq!(t.take(&key(2), 2 * HOUR), Err(Error::TableFull));
    assert_eq!(t.take(&key(2), 12 * HOUR), Ok(true));
}

#[test]
fn set_limit_keeps_counts_and_reset_clears_them() {
    let t = table(8, 2, HOUR, FullPolicy::Deny);
    for _ in 0..2 {
        assert_eq!(t.take(&key(1), 0), Ok(true));
    }
    assert_eq!(t.take(&key(1), 0), Ok(false));
    t.set_limit(3).unwrap();
    assert_eq!(t.take(&key(1), 0), Ok(true));
    assert_eq!(t.take(&key(1), 0), Ok(false));
    t.set_limit(1).unwrap();
    assert_eq!(t.count(&key(1), 0), Some(3));
    assert_eq!(t.take(&key(2), 0), Ok(true));
    t.reset(DAY).unwrap();
    assert_eq!(t.window_ns(), DAY);
    assert_eq!(t.count(&key(1), 0), None);
    let s = t.stats();
    assert_eq!((s.entries, s.removals), (0, 2));
    // New windows are a day long.
    assert_eq!(t.take(&key(1), 0), Ok(true));
    assert_eq!(t.take(&key(1), 2 * HOUR), Ok(false));
    assert_eq!(t.take(&key(1), DAY + DAY / 16), Ok(true));
    t.check_invariants().unwrap();
}

#[test]
fn sweep_removes_idle_clients_only() {
    // The 200 takes span 12.5 hours, so clients go idle while the table
    // fills; a full shard would reclaim them before the sweep could. The
    // hash seed is random, so with 256 entries (4 shards of 64, about 50
    // clients each) one sometimes filled: 11 runs in 300 reclaimed 2. At
    // 4,096 (64 shards, about 3 each) none can.
    let t = table(4096, 1, HOUR, FullPolicy::Deny);
    for i in 0..200 {
        assert_eq!(t.take(&key(i), u64::from(i) * SLOT), Ok(true));
    }
    let n = t.shards();
    let st = t.stats();
    assert_eq!(
        (st.entries, st.reclaims),
        (200, 0),
        "no shard filled: {st:?}"
    );
    assert_eq!(t.sweep(HOUR, n), 0, "every window still counts");
    // At an hour and two buckets, the clients seen in the first bucket
    // (0) and the second (1) have empty windows.
    assert_eq!(t.sweep(HOUR + 2 * SLOT, n), 2);
    assert_eq!(t.stats().entries, 198);
    assert_eq!(t.sweep(1_000 * HOUR, n), 198);
    let s = t.stats();
    assert_eq!((s.entries, s.removals), (0, 200));
    t.check_invariants().unwrap();
}

/// Inserts, reclaims and sweeps over many keys in a few slots keep the
/// index, the LRU list and the free list consistent, including probe
/// chains that wrap past the end of the index.
#[test]
fn churn_keeps_the_invariants() {
    let mut rng = Rng(7);
    let t = table(12, 1, HOUR, FullPolicy::Evict);
    assert_eq!(t.shards(), 1);
    let mut now = 0;
    for step in 0..20_000 {
        now += rng.below(SLOT);
        let _ = t.take(&key(rng.below(64) as u32), now);
        if step % 997 == 0 {
            // Long enough for every window to empty.
            now += 2 * HOUR;
            let before = t.stats().entries;
            assert_eq!(t.sweep(now, 1) as u64, before);
            assert_eq!(t.stats().entries, 0);
        }
        if step % 50 == 0 {
            t.check_invariants()
                .map_err(|e| format!("step {step}: {e}"))
                .unwrap();
        }
    }
    let s = t.stats();
    assert!(s.evictions > 0 && s.reclaims > 0 && s.removals > 0, "{s:?}");
    assert_eq!(s.inserts - s.reclaims - s.evictions - s.removals, s.entries);
}

#[test]
fn check_invariants_finds_broken_shards() {
    let t = table(4, 1, HOUR, FullPolicy::Deny);
    for i in 1..=3 {
        t.take(&key(i), 0).unwrap();
    }
    t.check_invariants().unwrap();
    let broken: [fn(&mut Shard); 6] = [
        |s| s.slots[s.slots.iter().position(|x| x.idx != NIL).unwrap()].idx = 99,
        |s| {
            let live: Vec<_> = s.slots.iter().filter(|x| x.idx != NIL).copied().collect();
            let i = s.slots.iter().position(|x| x.idx == live[1].idx).unwrap();
            s.slots[i].idx = live[0].idx;
            s.slots[i].tag = live[0].tag;
        },
        |s| {
            let i = s.slots.iter().position(|x| x.idx != NIL).unwrap();
            s.slots[i].tag ^= 1;
        },
        |s| s.head = s.tail,
        |s| s.free = s.head,
        |s| s.count += 1,
    ];
    for (n, f) in broken.into_iter().enumerate() {
        let t = table(4, 1, HOUR, FullPolicy::Deny);
        for i in 1..=3 {
            t.take(&key(i), 0).unwrap();
        }
        f(&mut lock(&t.shards[0]));
        assert!(t.check_invariants().is_err(), "case {n}");
    }
    // A free list that loops, and one short of the pool.
    let t = table(4, 1, HOUR, FullPolicy::Deny);
    lock(&t.shards[0]).pool[1].next = 0;
    assert!(t.check_invariants().is_err());
    let t = table(4, 1, HOUR, FullPolicy::Deny);
    lock(&t.shards[0]).pool[2].next = NIL;
    assert!(t.check_invariants().is_err());
}

#[test]
fn memory_is_fixed_at_creation() {
    let t = table(16_384, 1, DAY, FullPolicy::Evict);
    let m = t.memory();
    // The entries, plus an index of 8-byte slots at most 3/4 full.
    let entry = std::mem::size_of::<Entry>();
    assert_eq!(entry, 200, "docs/DESIGN.md gives this size");
    assert!(m >= 16_384 * (entry + 8), "{m}");
    assert!(m <= 16_384 * (entry + 24) + 4096 * 64, "{m}");
    for i in 0..40_000 {
        let _ = t.take(&key(i), u64::from(i));
    }
    assert_eq!(t.memory(), m);
    assert_eq!(t.stats().entries, 16_384);
    assert_eq!(t.shards(), 256);
    let d = format!("{t:?}");
    assert!(d.starts_with("QuotaTable { shards: 256, capacity: 16384, policy: Evict, limit: 1, window_ns: 86400000000000"), "{d}");
}

/// Threads racing on a few clients never admit more than the limit in one
/// window.
#[test]
fn racing_takes_never_exceed_the_limit() {
    const THREADS: usize = 8;
    const ROUNDS: usize = 2_000;
    let t = table(64, 100, HOUR, FullPolicy::Deny);
    let admitted = [(); 4].map(|_| AtomicUsize::new(0));
    let start = Barrier::new(THREADS);
    let errors = Mutex::new(Vec::new());
    thread::scope(|s| {
        for th in 0..THREADS {
            let (t, admitted, start, errors) = (&t, &admitted, &start, &errors);
            s.spawn(move || {
                start.wait();
                for r in 0..ROUNDS {
                    let k = (th + r) % 4;
                    match t.take(&key(k as u32), 0) {
                        Ok(true) => {
                            admitted[k].fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(false) => {}
                        Err(e) => errors.lock().unwrap().push(e),
                    }
                }
            });
        }
    });
    assert_eq!(*errors.lock().unwrap(), []);
    for (k, n) in admitted.iter().enumerate() {
        assert_eq!(n.load(Ordering::Relaxed), 100, "client {k}");
        assert_eq!(t.count(&key(k as u32), 0), Some(100));
    }
    t.check_invariants().unwrap();
}
