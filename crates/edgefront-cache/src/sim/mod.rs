//! Trace-driven cache simulation: replays a request trace against eviction
//! policies at several cache sizes and counts hits, bytes hit and writes.
//!
//! A [`Policy`] is the eviction algorithm alone, over 64-bit object ids and
//! byte sizes, with no entries, locks or headers, so a 100-million-request
//! trace replays in minutes. [`s3fifo`] is a model of the memory tier's
//! S3-FIFO (`store/memory.rs`) that makes the same decisions as one of its
//! shards, which a test holds it to; [`lru`] is the usual baseline; `sieve`
//! is the store's own SIEVE ([`crate::store::evict::sieve`]) over ids.
//!
//! [`trace`] reads traces as a stream, one record at a time: libCacheSim's
//! binary oracleGeneral format and CSV with configurable columns, either one
//! optionally gzip- or zstd-compressed. [`replay`] reads the trace once and
//! hands it in chunks to worker threads, each running some of the
//! simulations, so every (policy, size) pair sees the same requests in the
//! same order. [`cli`] is the `cache_sim` binary.

pub mod cli;
pub mod lru;
pub mod s3fifo;
pub mod trace;

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::io;
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::store::evict::sieve::{Inserted, Sieve};

/// One request of a trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    /// The trace's timestamp, in its own unit (seconds for oracleGeneral).
    pub time: u64,
    pub id: u64,
    /// Object size in bytes, at least 1.
    pub size: u64,
}

/// An eviction policy under simulation.
///
/// A request is a [`get`](Policy::get); on a miss the simulation calls
/// [`insert`](Policy::insert), as the cache stores what it fetched.
pub trait Policy: Send {
    /// The policy's name in results.
    fn name(&self) -> &'static str;
    /// Looks `id` up, counting the read as the real store does. True on a hit.
    fn get(&mut self, id: u64) -> bool;
    /// Stores `id` after a miss, evicting as needed. False when the policy
    /// refused it (too large for the cache).
    fn insert(&mut self, id: u64, size: u64) -> bool;
    /// Bytes held.
    fn bytes(&self) -> u64;
    /// Objects held.
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The policies the simulator knows, by name.
pub const POLICIES: &[&str] = &[s3fifo::NAME, lru::NAME, SIEVE];

/// SIEVE's name: the store's implementation, simulated as it is.
pub const SIEVE: &str = "sieve";

/// Builds the policy called `name` with a budget of `capacity` bytes.
pub fn policy(name: &str, capacity: u64) -> Option<Box<dyn Policy>> {
    match name {
        s3fifo::NAME => Some(Box::new(s3fifo::S3Fifo::new(capacity))),
        lru::NAME => Some(Box::new(lru::Lru::new(capacity))),
        SIEVE => Some(Box::new(Sieve::<u64, IdHasher>::with_hasher(
            capacity,
            IdHasher::default(),
        ))),
        _ => None,
    }
}

impl Policy for Sieve<u64, IdHasher> {
    fn name(&self) -> &'static str {
        SIEVE
    }

    fn get(&mut self, id: u64) -> bool {
        Sieve::get(self, &id)
    }

    fn insert(&mut self, id: u64, size: u64) -> bool {
        // A trace's miss is never for a held id, so this stores or refuses.
        Sieve::insert(self, id, size, |_, _| ()) == Inserted::Stored
    }

    fn bytes(&self) -> u64 {
        Sieve::bytes(self)
    }

    fn len(&self) -> usize {
        Sieve::len(self)
    }
}

/// What one simulation counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub requests: u64,
    pub hits: u64,
    pub bytes: u64,
    pub hit_bytes: u64,
    /// Misses the policy stored: what a write-through tier would write.
    pub writes: u64,
    pub write_bytes: u64,
}

impl Stats {
    pub fn hit_ratio(&self) -> f64 {
        ratio(self.hits, self.requests)
    }

    pub fn byte_hit_ratio(&self) -> f64 {
        ratio(self.hit_bytes, self.bytes)
    }
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

/// One policy at one cache size, and what it counted.
pub struct Simulation {
    pub capacity: u64,
    pub policy: Box<dyn Policy>,
    pub stats: Stats,
}

impl Simulation {
    pub fn new(policy: Box<dyn Policy>, capacity: u64) -> Self {
        Simulation {
            capacity,
            policy,
            stats: Stats::default(),
        }
    }

    pub fn access(&mut self, req: &Request) {
        let stats = &mut self.stats;
        stats.requests += 1;
        stats.bytes += req.size;
        if self.policy.get(req.id) {
            stats.hits += 1;
            stats.hit_bytes += req.size;
        } else if self.policy.insert(req.id, req.size) {
            stats.writes += 1;
            stats.write_bytes += req.size;
        }
    }
}

/// Requests per chunk handed to the workers.
pub const CHUNK: usize = 1 << 16;

/// What a replay did, beside each simulation's [`Stats`].
#[derive(Debug, Clone, PartialEq)]
pub struct Replay {
    pub requests: u64,
    pub elapsed: Duration,
}

/// Replays `trace` through every simulation, on up to `threads` worker
/// threads (at least one) while this thread reads. Each simulation sees
/// every request in trace order. Stops at the first read error, which it
/// returns; the simulations then hold what they counted until then.
pub fn replay<I>(trace: I, sims: &mut [Simulation], threads: usize) -> io::Result<Replay>
where
    I: Iterator<Item = io::Result<Request>>,
{
    let start = Instant::now();
    let threads = threads.clamp(1, sims.len().max(1));
    // Simulation i goes to worker i % threads, so a worker's share is
    // spread across policies and sizes rather than all the large caches.
    let mut groups: Vec<Vec<&mut Simulation>> = (0..threads).map(|_| Vec::new()).collect();
    for (i, sim) in sims.iter_mut().enumerate() {
        groups[i % threads].push(sim);
    }
    let mut requests = 0u64;
    let mut failed = None;
    std::thread::scope(|scope| {
        let mut senders = Vec::with_capacity(threads);
        for group in groups {
            // Two chunks in flight per worker keep it busy while the reader
            // fills the next one, and bound memory to a few chunks.
            let (tx, rx) = sync_channel::<Arc<Vec<Request>>>(2);
            senders.push(tx);
            scope.spawn(move || {
                let mut group = group;
                while let Ok(chunk) = rx.recv() {
                    for sim in group.iter_mut() {
                        for req in chunk.iter() {
                            sim.access(req);
                        }
                    }
                }
            });
        }
        let mut chunk = Vec::with_capacity(CHUNK);
        let mut trace = trace;
        loop {
            let next = trace.next();
            let done = match next {
                Some(Ok(req)) => {
                    chunk.push(req);
                    false
                }
                Some(Err(e)) => {
                    failed = Some(e);
                    true
                }
                None => true,
            };
            if chunk.len() == CHUNK || (done && !chunk.is_empty()) {
                requests += chunk.len() as u64;
                let full = Arc::new(std::mem::replace(&mut chunk, Vec::with_capacity(CHUNK)));
                for tx in &senders {
                    // A worker only stops when its sender is dropped.
                    tx.send(full.clone()).expect("simulation worker exited");
                }
            }
            if done {
                break;
            }
        }
        drop(senders);
    });
    match failed {
        Some(e) => Err(e),
        None => Ok(Replay {
            requests,
            elapsed: start.elapsed(),
        }),
    }
}

/// The distinct objects of a trace and their bytes, each counted at the
/// size it was first seen with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Footprint {
    pub requests: u64,
    pub objects: u64,
    pub bytes: u64,
}

/// Reads a whole trace to measure its footprint, for cache sizes given as a
/// share of it.
pub fn footprint<I>(trace: I) -> io::Result<Footprint>
where
    I: Iterator<Item = io::Result<Request>>,
{
    let mut seen: HashSet<u64, IdHasher> = HashSet::default();
    let mut out = Footprint::default();
    for req in trace {
        let req = req?;
        out.requests += 1;
        if seen.insert(req.id) {
            out.objects += 1;
            out.bytes += req.size;
        }
    }
    Ok(out)
}

/// A map keyed by object id.
pub(crate) type IdMap<V> = HashMap<u64, V, IdHasher>;
pub(crate) type IdSet = HashSet<u64, IdHasher>;

/// Hashing for object ids: SplitMix64's finaliser, which spreads sequential
/// and strided ids alike over every bit and costs a few cycles, where the
/// standard library's SipHash would dominate a replay. Ids are not
/// attacker-controlled here: they come from a trace file.
pub(crate) type IdHasher = BuildHasherDefault<SplitMix>;

#[derive(Default)]
pub(crate) struct SplitMix(u64);

impl Hasher for SplitMix {
    fn finish(&self) -> u64 {
        mix64(self.0)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 ^= n;
    }
}

pub(crate) fn mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(id: u64, size: u64) -> Request {
        Request { time: 0, id, size }
    }

    fn trace(reqs: &[Request]) -> impl Iterator<Item = io::Result<Request>> + '_ {
        reqs.iter().copied().map(Ok)
    }

    #[test]
    fn every_policy_is_built_by_name() {
        for name in POLICIES {
            let p = policy(name, 1_000).unwrap();
            assert_eq!(p.name(), *name);
            assert!(p.is_empty());
            assert_eq!(p.bytes(), 0);
        }
        assert!(policy("clock-pro", 1_000).is_none());
    }

    #[test]
    fn sieve_is_the_stores_own() {
        let mut p = policy(SIEVE, 10).unwrap();
        assert!(!p.get(1));
        assert!(p.insert(1, 4));
        assert!(!p.insert(2, 11), "larger than the cache");
        assert!(p.insert(2, 6));
        assert!(p.get(1));
        // 1 was read, so the hand spares it and takes 2.
        assert!(p.insert(3, 6));
        assert!(p.get(1) && !p.get(2) && p.get(3));
        assert_eq!((p.bytes(), p.len()), (10, 2));
    }

    #[test]
    fn a_simulation_counts_hits_bytes_and_writes() {
        let mut sim = Simulation::new(policy(lru::NAME, 100).unwrap(), 100);
        for r in [req(1, 10), req(1, 10), req(2, 500), req(2, 500), req(1, 10)] {
            sim.access(&r);
        }
        // 2 is larger than the cache, so it is never stored and never hits.
        assert_eq!(
            sim.stats,
            Stats {
                requests: 5,
                hits: 2,
                bytes: 1_030,
                hit_bytes: 20,
                writes: 1,
                write_bytes: 10,
            }
        );
        assert!((sim.stats.hit_ratio() - 0.4).abs() < 1e-12);
        assert!((sim.stats.byte_hit_ratio() - 20.0 / 1_030.0).abs() < 1e-12);
        assert_eq!(Stats::default().hit_ratio(), 0.0);
        assert_eq!(Stats::default().byte_hit_ratio(), 0.0);
    }

    fn sims() -> Vec<Simulation> {
        let mut out = Vec::new();
        for name in POLICIES {
            for cap in [50, 200, 1_000, 5_000] {
                out.push(Simulation::new(policy(name, cap).unwrap(), cap));
            }
        }
        out
    }

    fn workload(n: u64) -> Vec<Request> {
        // Skewed ids over a few sizes, longer than several chunks.
        (0..n)
            .map(|i| {
                let id = mix64(i) % 1_000;
                let id = id * id / 1_000;
                req(id, 1 + id % 17)
            })
            .collect()
    }

    #[test]
    fn replay_matches_a_sequential_run_on_any_thread_count() {
        let reqs = workload(3 * CHUNK as u64 + 123);
        let mut expect = sims();
        for sim in &mut expect {
            for r in &reqs {
                sim.access(r);
            }
        }
        for threads in [0, 1, 3, 64] {
            let mut got = sims();
            let replayed = replay(trace(&reqs), &mut got, threads).unwrap();
            assert_eq!(replayed.requests, reqs.len() as u64);
            for (g, e) in got.iter().zip(&expect) {
                assert_eq!(g.stats, e.stats, "{} at {}", g.policy.name(), g.capacity);
                assert_eq!(g.policy.bytes(), e.policy.bytes());
                assert_eq!(g.policy.len(), e.policy.len());
            }
        }
    }

    #[test]
    fn replay_of_nothing() {
        let mut got = sims();
        assert_eq!(replay(trace(&[]), &mut got, 4).unwrap().requests, 0);
        assert!(got.iter().all(|s| s.stats == Stats::default()));
        assert_eq!(replay(trace(&[req(1, 1)]), &mut [], 4).unwrap().requests, 1);
    }

    #[test]
    fn replay_stops_at_a_read_error_with_what_it_had() {
        let reqs = workload(CHUNK as u64 + 10);
        let failing = reqs
            .iter()
            .copied()
            .map(Ok)
            .chain(std::iter::once(Err(io::Error::other("truncated"))))
            .chain(std::iter::once(Ok(req(1, 1))));
        let mut got = sims();
        let err = replay(failing, &mut got, 2).unwrap_err();
        assert_eq!(err.to_string(), "truncated");
        assert!(got.iter().all(|s| s.stats.requests == reqs.len() as u64));
    }

    #[test]
    fn footprint_counts_distinct_objects_at_their_first_size() {
        let reqs = [req(1, 10), req(2, 5), req(1, 99), req(3, 1)];
        let f = footprint(trace(&reqs)).unwrap();
        assert_eq!(
            f,
            Footprint {
                requests: 4,
                objects: 3,
                bytes: 16
            }
        );
        let failing = [Ok(req(1, 1)), Err(io::Error::other("bad"))];
        assert!(footprint(failing.into_iter()).is_err());
    }

    #[test]
    fn id_hashing_spreads_bits() {
        use std::hash::BuildHasher;
        let h = IdHasher::default();
        assert_ne!(h.hash_one(1u64), h.hash_one(2u64));
        // Strided ids still differ in the low bits a table indexes by.
        let low: HashSet<u64> = (0..256u64).map(|i| h.hash_one(i << 20) & 0xff).collect();
        assert!(low.len() > 128, "{} distinct low bytes", low.len());
        // Byte input (a string id) is accepted too.
        assert_ne!(h.hash_one("a"), h.hash_one("b"));
    }
}
