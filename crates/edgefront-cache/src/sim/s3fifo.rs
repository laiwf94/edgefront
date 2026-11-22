//! A model of the memory tier's S3-FIFO (`store/memory.rs`) for simulation.
//!
//! It keeps the store's rules and constants exactly: a small FIFO queue
//! taking new objects with a tenth of the bytes, a main FIFO queue, a ghost
//! queue of evicted ids bounded by the main queue's length (at least 64),
//! reads counted up to 3, an object read in the small queue promoted on its
//! way out, a ghost hit inserted straight into main, and the same choice of
//! which queue gives up the next victim. It models one shard: the store
//! splits its budget over shards by key hash, each running this. A test
//! replays random traces through both and requires the same hit or miss on
//! every request.
//!
//! What it leaves out is what a trace cannot exercise: replacing a stored
//! object, removal, purge by tag, and the stale queue items those leave.

use std::collections::VecDeque;

use super::{IdMap, IdSet, Policy};

pub const NAME: &str = "s3fifo";

/// The small queue's share of the bytes, in percent (`SMALL_PERCENT`).
const SMALL_PERCENT: u64 = 10;
/// The read count's cap (`MAX_FREQ`).
const MAX_FREQ: u8 = 3;
/// The ghost queue's floor (`MIN_GHOSTS`).
const MIN_GHOSTS: usize = 64;

struct Slot {
    size: u64,
    freq: u8,
}

pub struct S3Fifo {
    capacity: u64,
    map: IdMap<Slot>,
    small: VecDeque<u64>,
    main: VecDeque<u64>,
    ghost: VecDeque<u64>,
    ghost_set: IdSet,
    small_bytes: u64,
    main_bytes: u64,
}

impl S3Fifo {
    pub fn new(capacity: u64) -> Self {
        S3Fifo {
            capacity,
            map: IdMap::default(),
            small: VecDeque::new(),
            main: VecDeque::new(),
            ghost: VecDeque::new(),
            ghost_set: IdSet::default(),
            small_bytes: 0,
            main_bytes: 0,
        }
    }

    fn evict_to_capacity(&mut self) {
        while self.small_bytes + self.main_bytes > self.capacity {
            let small_share = self.capacity * SMALL_PERCENT / 100;
            // As the store: the small queue first when it is over its share
            // or main is empty, and the other queue when the first yields
            // nothing.
            type Evict = fn(&mut S3Fifo) -> bool;
            let (first, then): (Evict, Evict) = if (self.small_bytes >= small_share
                && !self.small.is_empty())
                || self.main.is_empty()
            {
                (S3Fifo::evict_small, S3Fifo::evict_main)
            } else {
                (S3Fifo::evict_main, S3Fifo::evict_small)
            };
            if !(first(self) || then(self)) {
                break;
            }
        }
    }

    fn evict_small(&mut self) -> bool {
        while let Some(id) = self.small.pop_front() {
            let slot = self.map.get_mut(&id).expect("queued ids are held");
            if slot.freq > 0 {
                slot.freq = 0;
                self.small_bytes -= slot.size;
                self.main_bytes += slot.size;
                self.main.push_back(id);
                continue;
            }
            let slot = self.map.remove(&id).expect("queued ids are held");
            self.small_bytes -= slot.size;
            self.remember(id);
            return true;
        }
        false
    }

    fn evict_main(&mut self) -> bool {
        while let Some(id) = self.main.pop_front() {
            let slot = self.map.get_mut(&id).expect("queued ids are held");
            if slot.freq > 0 {
                slot.freq -= 1;
                self.main.push_back(id);
                continue;
            }
            let slot = self.map.remove(&id).expect("queued ids are held");
            self.main_bytes -= slot.size;
            return true;
        }
        false
    }

    fn remember(&mut self, id: u64) {
        if self.ghost_set.insert(id) {
            self.ghost.push_back(id);
        }
        // The store counts the main queue's live objects; with no removals
        // that is the queue's length.
        let limit = self.main.len().max(MIN_GHOSTS);
        while self.ghost.len() > limit {
            if let Some(old) = self.ghost.pop_front() {
                self.ghost_set.remove(&old);
            }
        }
    }
}

impl Policy for S3Fifo {
    fn name(&self) -> &'static str {
        NAME
    }

    fn get(&mut self, id: u64) -> bool {
        match self.map.get_mut(&id) {
            Some(slot) => {
                slot.freq = (slot.freq + 1).min(MAX_FREQ);
                true
            }
            None => false,
        }
    }

    fn insert(&mut self, id: u64, size: u64) -> bool {
        if size > self.capacity || self.map.contains_key(&id) {
            return false;
        }
        if self.ghost_set.contains(&id) {
            self.main.push_back(id);
            self.main_bytes += size;
        } else {
            self.small.push_back(id);
            self.small_bytes += size;
        }
        self.map.insert(id, Slot { size, freq: 0 });
        self.evict_to_capacity();
        true
    }

    fn bytes(&self) -> u64 {
        self.small_bytes + self.main_bytes
    }

    fn len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use bytes::Bytes;
    use http::{HeaderMap, StatusCode};
    use proptest::prelude::*;

    use super::*;
    use crate::object::{CachedObject, Entry, Key};
    use crate::policy::Freshness;
    use crate::store::memory::MemoryStore;

    fn entry(body_len: usize) -> Arc<Entry> {
        let f = Freshness {
            lifetime: Duration::from_secs(60),
            corrected_initial_age: Duration::ZERO,
            response_time: SystemTime::UNIX_EPOCH,
        };
        let obj = CachedObject::new(
            StatusCode::OK,
            HeaderMap::new(),
            Bytes::from(vec![b'x'; body_len]),
            f,
            &HeaderMap::new(),
        );
        Arc::new(Entry::single(Arc::new(obj)))
    }

    /// Replays `trace` (object, body length) through a one-shard
    /// `MemoryStore` and the model, as the cache uses the store: a get, and
    /// an insert on a miss. Returns the first request they disagree on.
    fn diverges(capacity_objects: usize, trace: &[(u16, usize)]) -> Option<usize> {
        let unit = entry(0).size();
        let capacity = capacity_objects * (unit + 64);
        let store = MemoryStore::new(capacity, 1, capacity);
        let mut model = S3Fifo::new(capacity as u64);
        for (i, &(id, len)) in trace.iter().enumerate() {
            let key = id.to_string();
            let real = store.get(&key).is_some();
            if !real {
                let e = entry(len);
                let size = e.size() as u64;
                assert_eq!(
                    store.insert(Key::from(key.as_str()), e).stored,
                    size <= capacity as u64
                );
            }
            let size = entry(len).size() as u64;
            let modelled = model.get(u64::from(id));
            if !modelled {
                model.insert(u64::from(id), size);
            }
            if real != modelled
                || store.bytes() as u64 != model.bytes()
                || store.len() != model.len()
            {
                return Some(i);
            }
        }
        None
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Skewed ids (a few hot, many cold), each with a fixed size so a
        /// hit never changes one; enough distinct ids to fill the ghost
        /// queue past its floor.
        #[test]
        fn makes_the_stores_decisions(
            capacity in 4usize..120,
            picks in proptest::collection::vec((0u16..400, 0u16..400), 200..3_000),
        ) {
            let trace: Vec<(u16, usize)> = picks
                .into_iter()
                .map(|(a, b)| {
                    let id = a.min(b);
                    (id, usize::from(id % 7) * 16)
                })
                .collect();
            prop_assert_eq!(diverges(capacity, &trace), None);
        }
    }

    #[test]
    fn makes_the_stores_decisions_on_a_long_scan_with_a_hot_set() {
        // Long enough that the main queue and the ghost queue both cycle.
        let trace: Vec<(u16, usize)> = (0..20_000u32)
            .map(|i| {
                let id = if i % 3 == 0 { i % 50 } else { 50 + i % 3_000 };
                (id as u16, (id % 5) as usize * 40)
            })
            .collect();
        assert_eq!(diverges(200, &trace), None);
    }

    #[test]
    fn refuses_what_does_not_fit_and_what_it_holds() {
        let mut p = S3Fifo::new(100);
        assert!(!p.insert(1, 101));
        assert!(p.insert(1, 100));
        assert!(!p.insert(1, 10), "a held id is a hit, not an insert");
        assert_eq!(p.bytes(), 100);
        assert!(p.get(1));
        assert!(p.insert(2, 60));
        assert!(!p.get(1) || !p.get(2));
        assert!(p.bytes() <= 100);
    }

    #[test]
    fn a_scan_does_not_evict_a_hot_object() {
        let mut p = S3Fifo::new(10_000);
        p.insert(0, 1_000);
        for i in 1..1_000 {
            assert!(p.get(0));
            if !p.get(i) {
                p.insert(i, 1_000);
            }
        }
        assert!(p.get(0));
        assert!(p.ghost.len() <= p.main.len().max(MIN_GHOSTS));
        assert_eq!(p.ghost.len(), p.ghost_set.len());
    }
}
