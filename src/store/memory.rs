//! The memory tier: a sharded S3-FIFO cache with a byte budget.
//!
//! S3-FIFO (Yang et al., SOSP 2023) keeps a small FIFO queue for new
//! objects, a main FIFO queue for objects that were read again, and a ghost
//! queue of recently evicted key hashes. One-hit objects leave through the
//! small queue without disturbing the main queue, which is what makes the
//! policy resistant to scans. Reads only bump a two-bit counter, so a hit
//! takes a shared lock and never reorders anything.

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::object::{Entry, Key};

/// The small queue's share of a shard's bytes, in percent (the paper's 10%).
const SMALL_PERCENT: usize = 10;
const MAX_FREQ: u8 = 3;
const MIN_GHOSTS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Queue {
    Small,
    Main,
}

struct Slot {
    entry: Arc<Entry>,
    size: usize,
    freq: AtomicU8,
    queue: Queue,
    generation: u64,
}

struct Shard {
    map: HashMap<Key, Slot>,
    small: VecDeque<(Key, u64)>,
    main: VecDeque<(Key, u64)>,
    ghost: VecDeque<u64>,
    ghost_set: HashSet<u64>,
    tags: HashMap<Arc<str>, HashSet<Key>>,
    small_bytes: usize,
    main_bytes: usize,
    main_count: usize,
    capacity: usize,
    next_generation: u64,
}

/// What an insert did.
#[derive(Debug, Default)]
pub struct InsertOutcome {
    pub stored: bool,
    /// Entries pushed out to make room, for the caller to demote or drop.
    pub evicted: Vec<(Key, Arc<Entry>)>,
}

pub struct MemoryStore {
    shards: Box<[RwLock<Shard>]>,
    hasher: RandomState,
    max_object: usize,
}

fn read(lock: &RwLock<Shard>) -> RwLockReadGuard<'_, Shard> {
    // A panic elsewhere cannot leave a shard half-updated in a way that
    // matters more than losing the cache, so poisoning is ignored.
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write(lock: &RwLock<Shard>) -> RwLockWriteGuard<'_, Shard> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

impl MemoryStore {
    /// `capacity` bytes spread over `shards` shards. Objects larger than
    /// `max_object` or a shard's capacity are refused.
    pub fn new(capacity: usize, shards: usize, max_object: usize) -> Self {
        let shards = shards.max(1);
        let per_shard = capacity / shards;
        let shards = (0..shards)
            .map(|_| {
                RwLock::new(Shard {
                    map: HashMap::new(),
                    small: VecDeque::new(),
                    main: VecDeque::new(),
                    ghost: VecDeque::new(),
                    ghost_set: HashSet::new(),
                    tags: HashMap::new(),
                    small_bytes: 0,
                    main_bytes: 0,
                    main_count: 0,
                    capacity: per_shard,
                    next_generation: 0,
                })
            })
            .collect();
        MemoryStore {
            shards,
            hasher: RandomState::new(),
            max_object: max_object.min(per_shard),
        }
    }

    fn locate(&self, key: &str) -> (u64, &RwLock<Shard>) {
        let hash = self.hasher.hash_one(key);
        let index = (hash % self.shards.len() as u64) as usize;
        (hash, &self.shards[index])
    }

    /// Looks up an entry and counts the read.
    pub fn get(&self, key: &str) -> Option<Arc<Entry>> {
        let (_, shard) = self.locate(key);
        let shard = read(shard);
        let slot = shard.map.get(key)?;
        // A lost increment under contention only makes the count approximate.
        let freq = slot.freq.load(Ordering::Relaxed);
        if freq < MAX_FREQ {
            slot.freq.store(freq + 1, Ordering::Relaxed);
        }
        Some(slot.entry.clone())
    }

    /// Looks up an entry without counting the read.
    pub fn peek(&self, key: &str) -> Option<Arc<Entry>> {
        let (_, shard) = self.locate(key);
        read(shard).map.get(key).map(|s| s.entry.clone())
    }

    /// Inserts or replaces an entry, evicting as needed.
    pub fn insert(&self, key: Key, entry: Arc<Entry>) -> InsertOutcome {
        let size = entry.size();
        if size > self.max_object {
            return InsertOutcome::default();
        }
        let (hash, shard) = self.locate(&key);
        let mut shard = write(shard);
        shard.put(key, entry, size, hash);
        let mut evicted = Vec::new();
        shard.evict_to_capacity(&self.hasher, &mut evicted);
        shard.compact();
        InsertOutcome {
            stored: true,
            evicted,
        }
    }

    /// Removes an entry. Removed entries do not enter the ghost queue.
    pub fn remove(&self, key: &str) -> Option<Arc<Entry>> {
        let (_, shard) = self.locate(key);
        let mut shard = write(shard);
        let (key, slot) = shard.map.remove_entry(key)?;
        shard.account_removed(&key, &slot);
        Some(slot.entry)
    }

    /// Removes every entry carrying surrogate key `tag`.
    pub fn purge_tag(&self, tag: &str) -> Vec<(Key, Arc<Entry>)> {
        let mut out = Vec::new();
        for shard in &self.shards {
            let mut shard = write(shard);
            let Some(keys) = shard.tags.remove(tag) else {
                continue;
            };
            for key in keys {
                if let Some((key, slot)) = shard.map.remove_entry(&key) {
                    shard.account_removed(&key, &slot);
                    out.push((key, slot.entry));
                }
            }
        }
        out
    }

    /// Number of distinct surrogate keys indexed.
    pub fn tag_count(&self) -> usize {
        let mut tags = HashSet::new();
        for shard in &self.shards {
            tags.extend(read(shard).tags.keys().cloned());
        }
        tags.len()
    }

    /// Bytes currently charged across all shards.
    pub fn bytes(&self) -> usize {
        self.shards
            .iter()
            .map(|s| {
                let s = read(s);
                s.small_bytes + s.main_bytes
            })
            .sum()
    }

    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| read(s).map.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.shards.iter().map(|s| read(s).capacity).sum()
    }

    /// Every key, for tests and diagnostics.
    pub fn keys(&self) -> Vec<Key> {
        self.shards
            .iter()
            .flat_map(|s| read(s).map.keys().cloned().collect::<Vec<_>>())
            .collect()
    }
}

impl Shard {
    fn put(&mut self, key: Key, entry: Arc<Entry>, size: usize, hash: u64) {
        if let Some(slot) = self.map.get_mut(&key) {
            let old = slot.size;
            let old_entry = std::mem::replace(&mut slot.entry, entry.clone());
            slot.size = size;
            let queue = slot.queue;
            match queue {
                Queue::Small => self.small_bytes = self.small_bytes - old + size,
                Queue::Main => self.main_bytes = self.main_bytes - old + size,
            }
            self.unindex(&key, &old_entry);
            self.index(&key, &entry);
            return;
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        let queue = if self.ghost_set.contains(&hash) {
            self.main.push_back((key.clone(), generation));
            self.main_bytes += size;
            self.main_count += 1;
            Queue::Main
        } else {
            self.small.push_back((key.clone(), generation));
            self.small_bytes += size;
            Queue::Small
        };
        let slot = Slot {
            entry,
            size,
            freq: AtomicU8::new(0),
            queue,
            generation,
        };
        self.index(&key, &slot.entry);
        self.map.insert(key, slot);
    }

    fn index(&mut self, key: &Key, entry: &Entry) {
        for tag in entry.tags() {
            self.tags.entry(tag).or_default().insert(key.clone());
        }
    }

    fn unindex(&mut self, key: &Key, entry: &Entry) {
        for tag in entry.tags() {
            if let Some(keys) = self.tags.get_mut(&tag) {
                keys.remove(key);
                if keys.is_empty() {
                    self.tags.remove(&tag);
                }
            }
        }
    }

    fn account_removed(&mut self, key: &Key, slot: &Slot) {
        self.unindex(key, &slot.entry);
        match slot.queue {
            Queue::Small => self.small_bytes -= slot.size,
            Queue::Main => {
                self.main_bytes -= slot.size;
                self.main_count -= 1;
            }
        }
    }

    fn evict_to_capacity(&mut self, hasher: &RandomState, out: &mut Vec<(Key, Arc<Entry>)>) {
        while self.small_bytes + self.main_bytes > self.capacity {
            let small_share = self.capacity * SMALL_PERCENT / 100;
            let evicted = if (self.small_bytes >= small_share && !self.small.is_empty())
                || self.main.is_empty()
            {
                self.evict_small(hasher)
            } else {
                self.evict_main()
            };
            match evicted {
                Some(e) => out.push(e),
                None => break,
            }
        }
    }

    /// Takes the oldest live item of `queue`, skipping removed and replaced ones.
    fn pop_live(&mut self, queue: Queue) -> Option<Key> {
        loop {
            let (key, generation) = match queue {
                Queue::Small => self.small.pop_front()?,
                Queue::Main => self.main.pop_front()?,
            };
            if self
                .map
                .get(&key)
                .is_some_and(|s| s.generation == generation && s.queue == queue)
            {
                return Some(key);
            }
        }
    }

    /// Objects read while in the small queue move to the main queue; the
    /// rest are evicted and remembered in the ghost queue.
    fn evict_small(&mut self, hasher: &RandomState) -> Option<(Key, Arc<Entry>)> {
        while let Some(key) = self.pop_live(Queue::Small) {
            let slot = self.map.get_mut(&key).expect("pop_live returns live keys");
            if *slot.freq.get_mut() > 0 {
                *slot.freq.get_mut() = 0;
                slot.queue = Queue::Main;
                let (size, generation) = (slot.size, slot.generation);
                self.small_bytes -= size;
                self.main_bytes += size;
                self.main_count += 1;
                self.main.push_back((key, generation));
                continue;
            }
            let slot = self.map.remove(&key).expect("pop_live returns live keys");
            self.account_removed(&key, &slot);
            self.remember(hasher.hash_one(&*key));
            return Some((key, slot.entry));
        }
        None
    }

    /// Objects in the main queue get one more round per recorded read.
    fn evict_main(&mut self) -> Option<(Key, Arc<Entry>)> {
        while let Some(key) = self.pop_live(Queue::Main) {
            let slot = self.map.get_mut(&key).expect("pop_live returns live keys");
            let freq = slot.freq.get_mut();
            if *freq > 0 {
                *freq -= 1;
                let generation = slot.generation;
                self.main.push_back((key, generation));
                continue;
            }
            let slot = self.map.remove(&key).expect("pop_live returns live keys");
            self.account_removed(&key, &slot);
            return Some((key, slot.entry));
        }
        None
    }

    fn remember(&mut self, hash: u64) {
        if self.ghost_set.insert(hash) {
            self.ghost.push_back(hash);
        }
        let limit = self.main_count.max(MIN_GHOSTS);
        while self.ghost.len() > limit {
            if let Some(old) = self.ghost.pop_front() {
                self.ghost_set.remove(&old);
            }
        }
    }

    /// Drops queue items left behind by removals once they outnumber live keys.
    fn compact(&mut self) {
        if self.small.len() + self.main.len() <= 2 * self.map.len() + 64 {
            return;
        }
        let map = &self.map;
        let live = |(k, g): &(Key, u64)| map.get(k).is_some_and(|s| s.generation == *g);
        self.small
            .retain(|item| live(item) && map[&item.0].queue == Queue::Small);
        self.main
            .retain(|item| live(item) && map[&item.0].queue == Queue::Main);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use bytes::Bytes;
    use http::{HeaderMap, StatusCode};

    use super::*;
    use crate::object::CachedObject;
    use crate::policy::Freshness;

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

    fn key(s: &str) -> Key {
        Key::from(s)
    }

    #[test]
    fn insert_get_remove() {
        let store = MemoryStore::new(1 << 20, 4, 1 << 16);
        assert!(store.is_empty());
        let e = entry(100);
        let size = e.size();
        assert!(store.insert(key("a"), e).stored);
        assert_eq!(store.len(), 1);
        assert_eq!(store.bytes(), size);
        assert!(store.get("a").is_some());
        assert!(store.peek("a").is_some());
        assert!(store.get("b").is_none());
        assert!(store.remove("a").is_some());
        assert!(store.remove("a").is_none());
        assert_eq!(store.bytes(), 0);
        assert_eq!(store.capacity(), 1 << 20);
    }

    #[test]
    fn oversized_objects_are_refused() {
        let store = MemoryStore::new(10_000, 1, 1_000);
        let out = store.insert(key("big"), entry(2_000));
        assert!(!out.stored);
        assert!(store.is_empty());
        // max_object is clamped to a shard's capacity.
        let store = MemoryStore::new(4_000, 4, 1 << 20);
        assert!(!store.insert(key("big"), entry(2_000)).stored);
    }

    #[test]
    fn replacing_updates_bytes() {
        let store = MemoryStore::new(1 << 20, 1, 1 << 16);
        store.insert(key("a"), entry(100));
        store.get("a");
        let bigger = entry(500);
        let size = bigger.size();
        store.insert(key("a"), bigger);
        assert_eq!(store.bytes(), size);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn byte_budget_is_never_exceeded() {
        let store = MemoryStore::new(20_000, 1, 5_000);
        for i in 0..500 {
            let out = store.insert(key(&format!("k{i}")), entry(100 + (i * 37) % 1_500));
            assert!(out.stored);
            assert!(store.bytes() <= 20_000, "over budget after insert {i}");
            if i % 3 == 0 {
                store.get(&format!("k{}", i / 2));
            }
        }
    }

    #[test]
    fn scan_does_not_evict_a_hot_key() {
        // Room for about ten objects.
        let store = MemoryStore::new(10 * (entry(1_000).size()) + 10, 1, 1 << 16);
        store.insert(key("hot"), entry(1_000));
        for i in 0..1_000 {
            store.get("hot");
            store.insert(key(&format!("scan{i}")), entry(1_000));
        }
        assert!(store.get("hot").is_some(), "hot key was evicted by a scan");
    }

    #[test]
    fn evicted_entries_are_returned() {
        let one = entry(1_000).size();
        let store = MemoryStore::new(3 * one, 1, one);
        let mut evicted = Vec::new();
        for i in 0..10 {
            evicted.extend(store.insert(key(&format!("k{i}")), entry(1_000)).evicted);
        }
        assert_eq!(evicted.len() + store.len(), 10);
        assert_eq!(&*evicted[0].0, "k0");
    }

    #[test]
    fn ghost_hit_goes_straight_to_main() {
        let one = entry(1_000).size();
        let store = MemoryStore::new(4 * one, 1, one);
        store.insert(key("a"), entry(1_000));
        for i in 0..4 {
            store.insert(key(&format!("f{i}")), entry(1_000));
        }
        assert!(store.peek("a").is_none(), "a should have been evicted");
        store.insert(key("a"), entry(1_000));
        let shard = read(&store.shards[0]);
        assert_eq!(shard.map["a"].queue, Queue::Main);
    }

    #[test]
    fn main_queue_gives_read_objects_another_round() {
        let one = entry(1_000).size();
        let store = MemoryStore::new(4 * one, 1, one);
        // Fill the main queue through ghost hits.
        for k in ["a", "b", "c"] {
            store.insert(key(k), entry(1_000));
        }
        for i in 0..8 {
            store.insert(key(&format!("f{i}")), entry(1_000));
        }
        for k in ["a", "b", "c"] {
            store.insert(key(k), entry(1_000));
        }
        store.get("a");
        store.get("a");
        for i in 0..20 {
            store.insert(key(&format!("g{i}")), entry(1_000));
        }
        assert!(store.peek("a").is_some());
    }

    #[test]
    fn removals_leave_no_unbounded_queue() {
        let store = MemoryStore::new(1 << 22, 1, 1 << 16);
        for i in 0..10_000 {
            let k = format!("k{}", i % 10);
            store.insert(key(&k), entry(10));
            store.remove(&k);
        }
        let shard = read(&store.shards[0]);
        assert!(shard.small.len() + shard.main.len() <= 2 * shard.map.len() + 65);
    }

    #[test]
    fn ghost_queue_is_bounded() {
        let one = entry(10).size();
        let store = MemoryStore::new(2 * one, 1, one);
        for i in 0..5_000 {
            store.insert(key(&format!("k{i}")), entry(10));
        }
        let shard = read(&store.shards[0]);
        assert!(shard.ghost.len() <= MIN_GHOSTS.max(shard.main_count));
        assert_eq!(shard.ghost.len(), shard.ghost_set.len());
    }

    #[test]
    fn keys_lists_everything() {
        let store = MemoryStore::new(1 << 20, 8, 1 << 16);
        for i in 0..20 {
            store.insert(key(&format!("k{i}")), entry(10));
        }
        let mut keys: Vec<_> = store.keys().iter().map(|k| k.to_string()).collect();
        keys.sort();
        assert_eq!(keys.len(), 20);
    }

    #[test]
    fn concurrent_reads_writes_and_removals() {
        let store = Arc::new(MemoryStore::new(200_000, 8, 10_000));
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let store = store.clone();
                std::thread::spawn(move || {
                    for i in 0..5_000 {
                        let k = format!("k{}", (i * 7 + t * 13) % 300);
                        match i % 4 {
                            0 => {
                                store.insert(key(&k), entry(100 + (i % 900)));
                            }
                            1 => {
                                store.remove(&k);
                            }
                            _ => {
                                store.get(&k);
                            }
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(store.bytes() <= store.capacity());
        let counted: usize = store
            .keys()
            .iter()
            .filter_map(|k| store.peek(k))
            .map(|e| e.size())
            .sum();
        assert_eq!(counted, store.bytes());
    }
}
