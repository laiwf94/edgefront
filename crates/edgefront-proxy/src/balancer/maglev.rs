//! Maglev consistent hashing, ported from steer's `balancer.Maglev`.
//!
//! This is the lookup table of Google's Maglev (Eisenbud et al., NSDI 2016).
//! Every endpoint gets a pseudo-random permutation of the table's slots,
//! derived from its address alone. The endpoints take turns claiming their
//! next free slot until the table is full, and a key maps to
//! `table[key % size]`.
//!
//! Removing one endpoint of N gives its slots to the others and moves only a
//! small share of the rest, so most keys keep their endpoint. Weights are
//! honoured by giving an endpoint a turn in proportion to its weight
//! relative to the heaviest. A table depends only on the endpoints'
//! addresses and weights, so every proxy with the same configuration maps
//! keys the same way, and the tables built here are the same, slot for slot,
//! as steer's.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use super::Policy;
use super::rng::Rng;

/// The default table size: a prime, as Maglev requires, and Envoy's
/// default. It should be at least 100 times the number of endpoints for an
/// even spread.
pub const DEFAULT_TABLE_SIZE: usize = 65_537;

/// How many tables are kept, one per distinct candidate set (in practice one
/// per priority level, plus the sets seen while endpoints fail).
pub const CACHE_SIZE: usize = 8;

/// The value of a slot no endpoint claimed. Only a table over no candidates,
/// or over candidates that all have weight zero, has one.
pub const EMPTY: i32 = -1;

/// An endpoint as Maglev sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The address its permutation is derived from.
    pub address: String,
    /// Its share of the table relative to the other candidates. An endpoint
    /// of weight zero never claims a slot.
    pub weight: u32,
}

impl Endpoint {
    /// An endpoint at `address` with this weight.
    pub fn new(address: impl Into<String>, weight: u32) -> Endpoint {
        Endpoint {
            address: address.into(),
            weight,
        }
    }
}

/// Why a Maglev could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The table size is not a prime larger than the number of endpoints.
    TableSize {
        /// The size asked for.
        size: usize,
        /// The number of endpoints.
        endpoints: usize,
    },
    /// There are more endpoints than a slot can name.
    TooManyEndpoints(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TableSize { size, .. } => write!(
                f,
                "balancer: maglev table size {size} must be a prime larger than the endpoint count"
            ),
            Error::TooManyEndpoints(n) => {
                write!(f, "balancer: maglev cannot index {n} endpoints")
            }
        }
    }
}

impl std::error::Error for Error {}

/// A lookup table: the endpoint index of every slot.
#[derive(Debug)]
struct Table {
    cands: Box<[usize]>,
    entries: Box<[i32]>,
}

/// The tables built so far, most recently inserted last.
#[derive(Debug, Default)]
struct Cache {
    tables: Vec<Arc<Table>>,
    /// The table the last keyed pick used, checked first.
    last: Option<Arc<Table>>,
}

/// Maglev consistent hashing over a fixed set of endpoints.
pub struct Maglev {
    weights: Box<[u32]>,
    size: usize,
    /// Each endpoint's permutation: slot `j` is `(offset + j * skip) % size`.
    offset: Box<[u64]>,
    skip: Box<[u64]>,
    rng: Arc<dyn Rng>,
    cache: Mutex<Cache>,
}

impl fmt::Debug for Maglev {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Maglev")
            .field("endpoints", &self.weights.len())
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// Hashes a request key, such as a header value, for [`Maglev`]'s picks:
/// steer's `balancer.HashKey`.
pub fn hash_key(key: &[u8]) -> u64 {
    // FNV-1a, then the SplitMix64 finalizer, so short and similar strings
    // spread over all 64 bits.
    let mut x: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in key {
        x ^= u64::from(b);
        x = x.wrapping_mul(0x0100_0000_01b3);
    }
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn hash_parts(prefix: &str, address: &str) -> u64 {
    let mut key = Vec::with_capacity(prefix.len() + 1 + address.len());
    key.extend_from_slice(prefix.as_bytes());
    key.push(0);
    key.extend_from_slice(address.as_bytes());
    hash_key(&key)
}

/// Whether `n` is prime, by trial division as steer checks it.
pub fn is_prime(n: usize) -> bool {
    if n < 2 {
        return false;
    }
    let mut d = 2usize;
    while d.saturating_mul(d) <= n {
        if n.is_multiple_of(d) {
            return false;
        }
        d += 1;
    }
    true
}

impl Maglev {
    /// A Maglev over `endpoints` with a table of `size` slots, which must be
    /// a prime larger than the number of endpoints. Keyless picks draw from
    /// `rng`.
    pub fn new(endpoints: &[Endpoint], size: usize, rng: Arc<dyn Rng>) -> Result<Maglev, Error> {
        let n = endpoints.len();
        if size <= n || !is_prime(size) {
            return Err(Error::TableSize { size, endpoints: n });
        }
        // Slots hold endpoint indices as i32, as steer's do.
        if i32::try_from(n).is_err() {
            return Err(Error::TooManyEndpoints(n));
        }
        let size64 = size as u64;
        Ok(Maglev {
            weights: endpoints.iter().map(|e| e.weight).collect(),
            size,
            offset: endpoints
                .iter()
                .map(|e| hash_parts("maglev-offset", &e.address) % size64)
                .collect(),
            skip: endpoints
                .iter()
                .map(|e| hash_parts("maglev-skip", &e.address) % (size64 - 1) + 1)
                .collect(),
            rng,
            cache: Mutex::new(Cache::default()),
        })
    }

    /// The number of slots in each table.
    pub fn size(&self) -> usize {
        self.size
    }

    /// How many endpoints the policy covers.
    pub fn len(&self) -> usize {
        self.weights.len()
    }

    /// Whether it covers no endpoints.
    pub fn is_empty(&self) -> bool {
        self.weights.is_empty()
    }

    /// Builds the lookup table over the endpoint indices `cands`: the
    /// paper's Algorithm 1 with weights, as steer's `Populate`. In round
    /// `r` an endpoint of weight `w` takes a turn when `r * w / max_w`
    /// grows, so over the whole table it claims slots in proportion to `w`.
    ///
    /// Every slot holds an index from `cands`. The table is all [`EMPTY`]
    /// when `cands` is empty or every candidate has weight zero; steer
    /// divides by zero on the latter.
    ///
    /// # Panics
    ///
    /// If a candidate is not an index of the endpoints.
    pub fn populate(&self, cands: &[usize]) -> Box<[i32]> {
        let mut entries = vec![EMPTY; self.size].into_boxed_slice();
        let max_w = cands.iter().map(|&i| self.weights[i]).max().unwrap_or(0);
        if max_w == 0 {
            return entries;
        }
        let max_w = u64::from(max_w);
        let size = self.size as u64;
        // Each candidate's next slot, (offset + j * skip) % size for its
        // j-th claim attempt, stepped by one addition. Both terms are below
        // size, so the sum cannot wrap. steer computes the product in
        // uint64, which gives the same slots for every table under 2^32.
        let mut pos: Vec<u64> = cands.iter().map(|&i| self.offset[i]).collect();
        let step = |p: &mut u64, skip: u64| {
            *p += skip;
            if *p >= size {
                *p -= size;
            }
        };
        let mut filled = 0;
        let mut round: u64 = 1;
        loop {
            for (k, &i) in cands.iter().enumerate() {
                let w = u64::from(self.weights[i]);
                // The heaviest take a turn every round.
                if w != max_w && round * w / max_w == (round - 1) * w / max_w {
                    continue;
                }
                let skip = self.skip[i];
                let p = &mut pos[k];
                while entries[*p as usize] >= 0 {
                    step(p, skip);
                }
                // new() checked that every index fits.
                entries[*p as usize] = i as i32;
                step(p, skip);
                filled += 1;
                if filled == self.size {
                    return entries;
                }
            }
            round += 1;
        }
    }

    /// The cached table for `cands`, built if needed.
    fn table(&self, cands: &[usize]) -> Arc<Table> {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(t) = &cache.last
            && *t.cands == *cands
        {
            return Arc::clone(t);
        }
        let t = match cache.tables.iter().find(|t| *t.cands == *cands) {
            Some(t) => Arc::clone(t),
            None => {
                let t = Arc::new(Table {
                    cands: cands.into(),
                    entries: self.populate(cands),
                });
                if cache.tables.len() == CACHE_SIZE {
                    cache.tables.remove(0);
                }
                cache.tables.push(Arc::clone(&t));
                t
            }
        };
        cache.last = Some(Arc::clone(&t));
        t
    }

    /// How many tables are cached.
    pub fn cached_tables(&self) -> usize {
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache.tables.len()
    }
}

impl Policy for Maglev {
    /// With a key, the endpoint owning slot `key % size` of the table over
    /// `cands`; when that endpoint was tried, the owner of the next slot
    /// that was not, walking forward, so retries are deterministic too.
    /// Without a key, a random candidate, or the first untried one after it.
    fn pick(&self, cands: &[usize], tried: &[usize], key: Option<u64>) -> Option<usize> {
        if cands.is_empty() {
            return None;
        }
        let open = |i: usize| tried.is_empty() || !tried.contains(&i);
        let Some(key) = key else {
            let start = self.rng.below(cands.len());
            return (0..cands.len())
                .map(|k| cands[(start + k) % cands.len()])
                .find(|&i| open(i));
        };
        let t = self.table(cands);
        let start = (key % self.size as u64) as usize;
        let (head, tail) = t.entries.split_at(start);
        tail.iter()
            .chain(head)
            .find(|&&e| e >= 0 && open(e as usize))
            .map(|&e| e as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::balancer::rng::SplitMix64;

    fn same(n: usize, weight: u32) -> Vec<Endpoint> {
        (0..n)
            .map(|i| Endpoint::new(format!("10.0.0.{}:80", i + 1), weight))
            .collect()
    }

    fn maglev(eps: &[Endpoint], size: usize) -> Maglev {
        Maglev::new(eps, size, Arc::new(SplitMix64::new(7))).unwrap()
    }

    fn all(n: usize) -> Vec<usize> {
        (0..n).collect()
    }

    // TestMaglevSpreadsByWeight.
    #[test]
    fn spreads_by_weight() {
        let eps: Vec<Endpoint> = [1, 1, 2, 4]
            .iter()
            .enumerate()
            .map(|(i, &w)| Endpoint::new(format!("10.0.0.{}:80", i + 1), w))
            .collect();
        let m = maglev(&eps, DEFAULT_TABLE_SIZE);
        let table = m.populate(&all(4));
        let mut counts = [0usize; 4];
        for &e in table.iter() {
            counts[e as usize] += 1;
        }
        let want = [1.0 / 8.0, 1.0 / 8.0, 2.0 / 8.0, 4.0 / 8.0];
        for i in 0..4 {
            let share = counts[i] as f64 / table.len() as f64;
            assert!(
                (share - want[i]).abs() <= 0.01,
                "endpoint {i} share {share:.4} want {:.4} (counts {counts:?})",
                want[i]
            );
        }
    }

    // TestMaglevMinimalDisruption: removing one endpoint of ten moves its
    // slots and almost no others; a mod-N hash would move about 90% of keys.
    #[test]
    fn minimal_disruption() {
        let m = maglev(&same(10, 1), DEFAULT_TABLE_SIZE);
        let before = m.populate(&all(10));
        let rest: Vec<usize> = (0..10).filter(|&i| i != 5).collect();
        let after = m.populate(&rest);
        let mut moved_others = 0;
        for (b, a) in before.iter().zip(after.iter()) {
            assert_ne!(*a, 5, "removed endpoint still in table");
            if b != a && *b != 5 {
                moved_others += 1;
            }
        }
        let share = moved_others as f64 / before.len() as f64;
        assert!(
            share <= 0.03,
            "{:.2}% moved between survivors",
            100.0 * share
        );
        let mod_moved = (0..10_000)
            .map(|k| hash_key(format!("user-{k}").as_bytes()))
            .filter(|h| h % 10 != h % 9)
            .count();
        assert!(mod_moved >= 8_000, "mod-N moved only {mod_moved}");
    }

    // TestMaglevKeysAreStableAndRetriesWalk.
    #[test]
    fn keys_are_stable_and_retries_walk() {
        let m = maglev(&same(3, 1), 101);
        let cands = all(3);
        let k = hash_key(b"alice");
        let first = m.pick(&cands, &[], Some(k)).unwrap();
        for _ in 0..10 {
            assert_eq!(m.pick(&cands, &[], Some(k)), Some(first));
        }
        let second = m.pick(&cands, &[first], Some(k)).unwrap();
        assert_ne!(second, first);
        assert_eq!(m.pick(&cands, &[first], Some(k)), Some(second));
        assert_eq!(m.pick(&cands, &[0, 1, 2], Some(k)), None);
        for _ in 0..20 {
            let got = m.pick(&cands, &[1], None).unwrap();
            assert_ne!(got, 1);
        }
        assert_eq!(m.pick(&cands, &[0, 1, 2], None), None);
        assert_eq!(m.pick(&[], &[], None), None);
        assert_eq!(m.pick(&[], &[], Some(k)), None);
    }

    #[test]
    fn a_key_goes_to_its_slot_and_retries_walk_forward() {
        let m = maglev(&same(5, 1), 13);
        let cands = all(5);
        let table = m.populate(&cands);
        for key in 0..200u64 {
            let s = (key % 13) as usize;
            let owner = table[s] as usize;
            assert_eq!(m.pick(&cands, &[], Some(key)), Some(owner), "key {key}");
            let next = (1..13)
                .map(|d| table[(s + d) % 13] as usize)
                .find(|&e| e != owner)
                .unwrap();
            assert_eq!(m.pick(&cands, &[owner], Some(key)), Some(next), "key {key}");
        }
    }

    // TestMaglevTableCacheEvicts.
    #[test]
    fn table_cache_evicts() {
        let m = maglev(&same(12, 1), 101);
        for i in 0..12 {
            assert_eq!(m.pick(&[i], &[], Some(7)), Some(i));
        }
        assert_eq!(m.cached_tables(), CACHE_SIZE);
        // Endpoint 0's table was evicted first and is rebuilt.
        assert_eq!(m.pick(&[0], &[], Some(7)), Some(0));
        assert_eq!(m.cached_tables(), CACHE_SIZE);
        // The first of the cached sets, found behind `last`.
        assert_eq!(m.pick(&[6], &[], Some(7)), Some(6));
        assert!(m.populate(&[]).iter().all(|&e| e == EMPTY));
    }

    // TestMaglevRejectsBadSize.
    #[test]
    fn rejects_bad_sizes() {
        let eps = same(2, 1);
        for size in [0, 1, 2, 100, 65_536] {
            let err = Maglev::new(&eps, size, Arc::new(SplitMix64::new(1))).unwrap_err();
            assert_eq!(err, Error::TableSize { size, endpoints: 2 });
            assert!(
                err.to_string()
                    .contains(&format!("size {size} must be a prime"))
            );
        }
        assert!(Maglev::new(&eps, 3, Arc::new(SplitMix64::new(1))).is_ok());
        assert_eq!(
            Error::TooManyEndpoints(1 << 31).to_string(),
            "balancer: maglev cannot index 2147483648 endpoints"
        );
    }

    #[test]
    fn primes() {
        let small: Vec<usize> = (0..60).filter(|&n| is_prime(n)).collect();
        assert_eq!(
            small,
            [
                2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59
            ]
        );
        assert!(is_prime(65_537) && !is_prime(65_535) && is_prime(2_147_483_647));
        assert!(!is_prime(usize::MAX));
    }

    #[test]
    fn every_slot_is_a_candidate() {
        let rng = SplitMix64::new(99);
        for _ in 0..200 {
            let n = 1 + rng.below(20);
            let eps: Vec<Endpoint> = (0..n)
                .map(|i| Endpoint::new(format!("h{i}:{}", rng.below(65_536)), rng.below(5) as u32))
                .collect();
            let m = maglev(&eps, 211);
            let cands: Vec<usize> = (0..n).filter(|_| rng.below(3) > 0).collect();
            let table = m.populate(&cands);
            if cands.iter().all(|&i| eps[i].weight == 0) {
                assert!(table.iter().all(|&e| e == EMPTY));
                assert_eq!(m.pick(&cands, &[], Some(5)), None);
                continue;
            }
            for &e in table.iter() {
                assert!(cands.contains(&(e as usize)), "slot holds {e}");
                assert_ne!(
                    eps[e as usize].weight, 0,
                    "a weight-0 endpoint claimed a slot"
                );
            }
        }
    }

    #[test]
    fn accessors_and_debug() {
        let m = maglev(&same(3, 1), 7);
        assert_eq!((m.size(), m.len(), m.is_empty()), (7, 3, false));
        assert!(maglev(&[], 2).is_empty());
        assert_eq!(format!("{m:?}"), "Maglev { endpoints: 3, size: 7, .. }");
        assert_eq!(Endpoint::new("a:1", 2).address, "a:1");
    }

    #[test]
    fn threads_share_the_cache() {
        let m = Arc::new(maglev(&same(16, 1), 211));
        // More candidate sets than the cache holds, so threads evict each
        // other's tables while they pick.
        let sets: Vec<Vec<usize>> = (0..12)
            .map(|s| (0..16).filter(|i| i % 12 != s).collect())
            .collect();
        let want: Vec<Box<[i32]>> = sets.iter().map(|c| m.populate(c)).collect();
        let sets = Arc::new(sets);
        let want = Arc::new(want);
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let (m, sets, want) = (Arc::clone(&m), Arc::clone(&sets), Arc::clone(&want));
                std::thread::spawn(move || {
                    for k in 0..2_000u64 {
                        let s = ((k + t) % 12) as usize;
                        let key = k.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ t;
                        let slot = (key % 211) as usize;
                        assert_eq!(
                            m.pick(&sets[s], &[], Some(key)),
                            Some(want[s][slot] as usize)
                        );
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(m.cached_tables(), CACHE_SIZE);
    }

    #[test]
    fn survives_a_poisoned_lock() {
        let m = Arc::new(maglev(&same(2, 1), 7));
        let p = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _guard = p.cache.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert!(m.pick(&[0, 1], &[], Some(3)).is_some());
        assert_eq!(m.cached_tables(), 1);
    }
}
