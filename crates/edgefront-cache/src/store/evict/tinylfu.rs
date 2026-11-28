//! W-TinyLFU eviction (Einziger, Friedman and Manes, "TinyLFU: A Highly
//! Efficient Cache Admission Policy", ACM ToS 2017), over byte sizes.
//!
//! The cache is split into three LRU regions:
//!
//! - the **window**, [`WINDOW_PERCENT`] of the budget, where every new
//!   object lands, so a burst of new objects gets a short life of its own
//!   without displacing anything that has proven itself;
//! - **probation**, which an object leaving the window enters as a
//!   *candidate*;
//! - **protected**, [`PROTECTED_PERCENT`] of the budget outside the window,
//!   which an object in probation joins on its next hit. When protected
//!   grows past its share its least recently used objects go back to
//!   probation.
//!
//! When the cache is over budget, each candidate fresh from the window
//! duels the least recently used object in probation, the victim: the
//! candidate is admitted (and the victim evicted) only if a count-min
//! sketch of recent accesses estimates it more popular. A candidate whose
//! estimate is at least [`HASH_FLOOD_THRESHOLD`] but not above the
//! victim's is still admitted one time in 128, at random, so an attacker
//! who raises a victim's estimate with colliding keys cannot pin it in the
//! cache forever. Once no fresh candidate is left (or only candidates are
//! left in probation), objects are evicted in LRU order from probation,
//! then protected, then the window.
//!
//! The sketch ([`FrequencySketch`]) holds four 4-bit counters per object
//! in each of four rows, each row's counters for one key in the same
//! 64-byte block so an access touches one cache line. After ten
//! increments per table word it halves every counter, so popularity
//! decays and a formerly hot set can be displaced. It is sized for the
//! objects held: it grows as the cache does (starting again from zero
//! counts, as a resize of any sketch must) and never shrinks, so it costs
//! 8 to 16 bytes per object.
//!
//! The regions are intrusive doubly linked lists over one dense slab of
//! nodes, indexed by a hash map, so a hit, an insert and an eviction are
//! O(1) amortised. A hit moves its object, so unlike SIEVE's
//! [`get`](crate::store::evict::sieve::Sieve::get) this one takes `&mut
//! self`.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash};

const NIL: u32 = u32::MAX;

/// The window's share of the budget, in percent.
pub const WINDOW_PERCENT: u64 = 1;
/// Protected's share of the budget outside the window, in percent.
pub const PROTECTED_PERCENT: u64 = 80;
/// The estimate from which a losing candidate is admitted at random.
pub const HASH_FLOOD_THRESHOLD: u8 = 6;

/// Where an object is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Window = 0,
    Probation = 1,
    Protected = 2,
}

struct Node<K> {
    key: K,
    /// The key's hash, for the sketch.
    hash: u64,
    size: u64,
    region: Region,
    /// Toward the region's most recently used end.
    newer: u32,
    /// Toward its least recently used end.
    older: u32,
}

#[derive(Debug, Clone, Copy)]
struct List {
    newest: u32,
    oldest: u32,
    bytes: u64,
}

const EMPTY: List = List {
    newest: NIL,
    oldest: NIL,
    bytes: 0,
};

/// What [`TinyLfu::insert`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inserted {
    /// The key was new and is now held, in the window.
    Stored,
    /// The key was held: its size is updated in place and the write counts
    /// as a read.
    Replaced,
    /// The object went in but lost its admission duel at once (it was
    /// larger than the window and less popular than the victim), and was
    /// handed back to the eviction callback with the others.
    Rejected,
    /// The object is larger than the whole capacity and was not stored. A
    /// replaced key whose new size is too large is removed.
    TooLarge,
}

/// A W-TinyLFU cache of keys holding up to `capacity` bytes.
pub struct TinyLfu<K, S = RandomState> {
    capacity: u64,
    window_cap: u64,
    protected_cap: u64,
    map: HashMap<K, u32, S>,
    nodes: Vec<Node<K>>,
    lists: [List; 3],
    sketch: FrequencySketch,
    /// SplitMix64 state for the hash-flooding jitter, seeded from the
    /// hasher so it is as unpredictable as the hasher is.
    rng: u64,
    /// During an eviction, the oldest candidate not yet decided.
    candidate: u32,
    /// During an insert, the slot of the object being inserted.
    watch: u32,
}

impl<K: Hash + Eq + Clone> TinyLfu<K> {
    pub fn new(capacity: u64) -> Self {
        Self::with_hasher(capacity, RandomState::new())
    }
}

impl<K: Hash + Eq + Clone, S: BuildHasher> TinyLfu<K, S> {
    pub fn with_hasher(capacity: u64, hasher: S) -> Self {
        let rng = hasher.hash_one(0x7419_1fe5_u64);
        let (window_cap, protected_cap) = shares(capacity);
        TinyLfu {
            capacity,
            window_cap,
            protected_cap,
            map: HashMap::with_hasher(hasher),
            nodes: Vec::new(),
            lists: [EMPTY; 3],
            sketch: FrequencySketch::new(0),
            rng,
            candidate: NIL,
            watch: NIL,
        }
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Bytes held.
    pub fn bytes(&self) -> u64 {
        self.lists.iter().map(|l| l.bytes).sum()
    }

    /// Bytes held in one region.
    pub fn region_bytes(&self, region: Region) -> u64 {
        self.lists[region as usize].bytes
    }

    /// Objects held.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The sketch's estimate of how often `key` was accessed lately, held
    /// or not.
    pub fn frequency<Q>(&self, key: &Q) -> u8
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.sketch.frequency(self.map.hasher().hash_one(key))
    }

    /// Looks `key` up and counts the read: the sketch counts it and the
    /// object moves up its region, or from probation to protected. True
    /// when it is held.
    pub fn get<Q>(&mut self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match self.map.get(key) {
            Some(&i) => {
                self.sketch.increment(self.nodes[i as usize].hash);
                self.on_hit(i);
                true
            }
            None => false,
        }
    }

    /// Whether `key` is held, without counting a read.
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.contains_key(key)
    }

    /// The region `key` is held in.
    pub fn region_of<Q>(&self, key: &Q) -> Option<Region>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.get(key).map(|&i| self.nodes[i as usize].region)
    }

    /// The size `key` is held at.
    pub fn size_of<Q>(&self, key: &Q) -> Option<u64>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.get(key).map(|&i| self.nodes[i as usize].size)
    }

    /// Stores `key` at `size` bytes, evicting until the cache is within
    /// budget and handing each evicted key and its size to `evicted`, in
    /// the order decided. The new object itself may be among them
    /// ([`Inserted::Rejected`]).
    pub fn insert(&mut self, key: K, size: u64, mut evicted: impl FnMut(K, u64)) -> Inserted {
        if let Some(&i) = self.map.get(&key) {
            if size > self.capacity {
                let (key, old) = self.remove_at(i);
                evicted(key, old);
                return Inserted::TooLarge;
            }
            let node = &mut self.nodes[i as usize];
            let list = &mut self.lists[node.region as usize];
            list.bytes = list.bytes - node.size + size;
            node.size = size;
            let hash = node.hash;
            self.sketch.increment(hash);
            self.on_hit(i);
            // A protected object that grew can push protected past its share.
            self.demote_protected();
            return self.settle(i, Inserted::Replaced, &mut evicted);
        }
        if size > self.capacity {
            return Inserted::TooLarge;
        }
        let hash = self.map.hasher().hash_one(&key);
        let i = u32::try_from(self.nodes.len())
            .ok()
            .filter(|&i| i != NIL)
            .expect("fewer than 2^32 - 1 objects held");
        self.nodes.push(Node {
            key: key.clone(),
            hash,
            size,
            region: Region::Window,
            newer: NIL,
            older: NIL,
        });
        self.map.insert(key, i);
        self.push_newest(Region::Window, i);
        self.sketch.ensure_capacity(self.nodes.len());
        self.sketch.increment(hash);
        self.settle(i, Inserted::Stored, &mut evicted)
    }

    /// Evicts to the budget after `i` was written, reporting `did` if `i`
    /// survived it and [`Inserted::Rejected`] if not.
    fn settle(&mut self, i: u32, did: Inserted, evicted: &mut impl FnMut(K, u64)) -> Inserted {
        self.watch = i;
        self.evict_to(self.capacity, evicted);
        let survived = self.watch != NIL;
        self.watch = NIL;
        if survived { did } else { Inserted::Rejected }
    }

    /// Removes `key`, as a purge does, returning the size it was held at.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<u64>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let &i = self.map.get(key)?;
        Some(self.remove_at(i).1)
    }

    /// Changes the byte budget and the regions' shares of it, evicting
    /// down to a smaller one.
    pub fn set_capacity(&mut self, capacity: u64, mut evicted: impl FnMut(K, u64)) {
        self.capacity = capacity;
        (self.window_cap, self.protected_cap) = shares(capacity);
        self.demote_protected();
        self.evict_to(capacity, &mut evicted);
    }

    fn on_hit(&mut self, i: u32) {
        let region = self.nodes[i as usize].region;
        self.unlink(i);
        match region {
            Region::Window => self.push_newest(Region::Window, i),
            Region::Protected => self.push_newest(Region::Protected, i),
            Region::Probation => {
                self.push_newest(Region::Protected, i);
                self.demote_protected();
            }
        }
    }

    /// Moves protected's least recently used objects to probation until it
    /// is within its share.
    fn demote_protected(&mut self) {
        while self.lists[Region::Protected as usize].bytes > self.protected_cap {
            let j = self.lists[Region::Protected as usize].oldest;
            self.unlink(j);
            self.push_newest(Region::Probation, j);
        }
    }

    /// Moves the window's overflow to probation as candidates, then evicts
    /// until at most `limit` bytes are held.
    fn evict_to(&mut self, limit: u64, evicted: &mut impl FnMut(K, u64)) {
        self.candidate = NIL;
        while self.lists[Region::Window as usize].bytes > self.window_cap {
            let j = self.lists[Region::Window as usize].oldest;
            self.unlink(j);
            self.push_newest(Region::Probation, j);
            if self.candidate == NIL {
                self.candidate = j;
            }
        }
        while self.bytes() > limit {
            let candidate = self.candidate;
            let victim = self.lists[Region::Probation as usize].oldest;
            let out = if candidate != NIL && victim != candidate {
                let cf = self.sketch.frequency(self.nodes[candidate as usize].hash);
                let vf = self.sketch.frequency(self.nodes[victim as usize].hash);
                if self.admit(cf, vf) {
                    // The candidate stays; the next one duels the next victim.
                    self.candidate = self.nodes[candidate as usize].newer;
                    victim
                } else {
                    // remove_at moves the candidate on to the next newer.
                    candidate
                }
            } else {
                // Only candidates are left in probation, or none were made:
                // plain LRU order.
                self.candidate = NIL;
                [Region::Probation, Region::Protected, Region::Window]
                    .into_iter()
                    .map(|r| self.lists[r as usize].oldest)
                    .find(|&j| j != NIL)
                    .expect("over budget, so something is held")
            };
            let (key, size) = self.remove_at(out);
            evicted(key, size);
        }
        self.candidate = NIL;
    }

    fn admit(&mut self, candidate: u8, victim: u8) -> bool {
        if candidate > victim {
            true
        } else if candidate >= HASH_FLOOD_THRESHOLD {
            self.rng = self.rng.wrapping_add(GOLDEN);
            spread(self.rng) & 127 == 0
        } else {
            false
        }
    }

    fn unlink(&mut self, i: u32) {
        let (newer, older, size, region) = {
            let n = &self.nodes[i as usize];
            (n.newer, n.older, n.size, n.region)
        };
        let list = &mut self.lists[region as usize];
        list.bytes -= size;
        match newer {
            NIL => list.newest = older,
            n => self.nodes[n as usize].older = older,
        }
        let list = &mut self.lists[region as usize];
        match older {
            NIL => list.oldest = newer,
            o => self.nodes[o as usize].newer = newer,
        }
    }

    fn push_newest(&mut self, region: Region, i: u32) {
        let list = &mut self.lists[region as usize];
        let old = list.newest;
        list.newest = i;
        if old == NIL {
            list.oldest = i;
        }
        list.bytes += self.nodes[i as usize].size;
        let n = &mut self.nodes[i as usize];
        n.region = region;
        n.newer = NIL;
        n.older = old;
        if old != NIL {
            self.nodes[old as usize].newer = i;
        }
    }

    /// Unlinks slot `i` and fills it with the last node.
    fn remove_at(&mut self, i: u32) -> (K, u64) {
        if self.candidate == i {
            self.candidate = self.nodes[i as usize].newer;
        }
        if self.watch == i {
            self.watch = NIL;
        }
        self.unlink(i);
        let last = u32::try_from(self.nodes.len() - 1).expect("slots fit in u32");
        let node = self.nodes.swap_remove(i as usize);
        self.map.remove(&node.key);
        if i != last {
            // The node that was last now lives in slot i: repoint its
            // neighbours, its region's ends, the cursors and its map entry.
            let (newer, older, region) = {
                let n = &self.nodes[i as usize];
                (n.newer, n.older, n.region)
            };
            match newer {
                NIL => self.lists[region as usize].newest = i,
                n => self.nodes[n as usize].older = i,
            }
            match older {
                NIL => self.lists[region as usize].oldest = i,
                o => self.nodes[o as usize].newer = i,
            }
            if self.candidate == last {
                self.candidate = i;
            }
            if self.watch == last {
                self.watch = i;
            }
            *self
                .map
                .get_mut(&self.nodes[i as usize].key)
                .expect("every node is mapped") = i;
        }
        (node.key, node.size)
    }

    /// Each region's keys and sizes, least recently used first, checking
    /// the links, byte counts and map on the way.
    #[cfg(test)]
    fn regions(&self) -> [Vec<(K, u64)>; 3] {
        let mut out: [Vec<(K, u64)>; 3] = Default::default();
        for (r, list) in self.lists.iter().enumerate() {
            let mut i = list.oldest;
            let mut prev = NIL;
            let mut bytes = 0;
            while i != NIL {
                let n = &self.nodes[i as usize];
                assert_eq!(n.older, prev, "links agree");
                assert_eq!(n.region as usize, r, "node in its region");
                assert_eq!(self.map[&n.key], i, "the map points at the node");
                assert_eq!(n.hash, self.map.hasher().hash_one(&n.key));
                out[r].push((n.key.clone(), n.size));
                bytes += n.size;
                prev = i;
                i = n.newer;
            }
            assert_eq!(prev, list.newest);
            assert_eq!(bytes, list.bytes);
        }
        assert_eq!(out.iter().map(Vec::len).sum::<usize>(), self.nodes.len());
        assert_eq!(self.map.len(), self.nodes.len());
        out
    }
}

/// The window's and protected's budgets for a total of `capacity` bytes.
fn shares(capacity: u64) -> (u64, u64) {
    let window = capacity / 100 * WINDOW_PERCENT + capacity % 100 * WINDOW_PERCENT / 100;
    let main = u128::from(capacity - window);
    let protected = main * u128::from(PROTECTED_PERCENT) / 100;
    (window, protected as u64)
}

const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;

/// SplitMix64's finaliser.
fn spread(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A count-min sketch of 4-bit counters with periodic halving, estimating
/// how often each key was seen lately.
///
/// The table is a power of two of 64-bit words, each holding sixteen
/// counters, grouped in 64-byte blocks of eight words. A key's hash picks
/// one block, and in it each of four rows has its own two words, from
/// which the hash picks one counter: a key's four counters share a cache
/// line but never a counter. The estimate is the smallest of the four. An
/// increment raises each of them that is below 15, and once `10 x words`
/// increments have raised one, every counter is halved.
pub struct FrequencySketch {
    table: Vec<u64>,
    additions: u64,
    sample: u64,
    resets: u64,
}

const BLOCK_WORDS: usize = 8;
const MIN_WORDS: usize = 64;
const MAX_WORDS: usize = 1 << 30;
const SAMPLE_PER_WORD: u64 = 10;
const ONE_MASK: u64 = 0x1111_1111_1111_1111;
const RESET_MASK: u64 = 0x7777_7777_7777_7777;

impl FrequencySketch {
    /// A sketch for about `objects` keys: one word (sixteen counters) per
    /// key, rounded up to a power of two, between 64 and 2^30 words.
    pub fn new(objects: usize) -> Self {
        let words = objects.clamp(MIN_WORDS, MAX_WORDS).next_power_of_two();
        FrequencySketch {
            table: vec![0; words],
            additions: 0,
            sample: SAMPLE_PER_WORD * words as u64,
            resets: 0,
        }
    }

    /// Grows the table, with every count back at zero, when it was sized
    /// for fewer than `objects` keys.
    pub fn ensure_capacity(&mut self, objects: usize) {
        if objects > self.table.len() && self.table.len() < MAX_WORDS {
            let resets = self.resets;
            *self = FrequencySketch::new(objects);
            self.resets = resets;
        }
    }

    /// Words in the table.
    pub fn words(&self) -> usize {
        self.table.len()
    }

    /// Times every counter was halved.
    pub fn resets(&self) -> u64 {
        self.resets
    }

    /// The word and bit shift of `hash`'s counter in `row`.
    fn slot(&self, spread_hash: u64, row: usize) -> (usize, u32) {
        let blocks = self.table.len() / BLOCK_WORDS;
        let block = (spread_hash as usize) & (blocks - 1);
        let sel = (spread_hash >> (32 + 8 * row)) & 0x1f;
        let word = block * BLOCK_WORDS + row * 2 + (sel >> 4) as usize;
        (word, ((sel & 0xf) as u32) * 4)
    }

    /// The estimated count for `hash`, from 0 to 15.
    pub fn frequency(&self, hash: u64) -> u8 {
        let h = spread(hash);
        (0..4)
            .map(|row| {
                let (w, shift) = self.slot(h, row);
                ((self.table[w] >> shift) & 0xf) as u8
            })
            .min()
            .expect("four rows")
    }

    /// Counts one access to `hash`. True when any counter rose.
    pub fn increment(&mut self, hash: u64) -> bool {
        let h = spread(hash);
        let mut added = false;
        for row in 0..4 {
            let (w, shift) = self.slot(h, row);
            if (self.table[w] >> shift) & 0xf < 15 {
                self.table[w] += 1 << shift;
                added = true;
            }
        }
        if added {
            self.additions += 1;
            if self.additions >= self.sample {
                self.reset();
            }
        }
        added
    }

    /// Halves every counter. A counter that was odd loses half a count;
    /// with four counters per key, a quarter of those is the number of
    /// additions the halving lost beyond half.
    fn reset(&mut self) {
        let mut odd = 0u64;
        for w in &mut self.table {
            odd += u64::from((*w & ONE_MASK).count_ones());
            *w = (*w >> 1) & RESET_MASK;
        }
        self.additions = self.additions.saturating_sub(odd / 4) / 2;
        self.resets += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::BuildHasherDefault;

    use proptest::prelude::*;

    use super::*;

    type Fixed = BuildHasherDefault<DefaultHasher>;

    fn fixed() -> Fixed {
        Fixed::default()
    }

    /// W-TinyLFU as its description reads, over vectors (least recently
    /// used first): the obvious O(n) version, using its own sketch of the
    /// same kind (tested on its own below) and the same jitter.
    struct Reference {
        capacity: u64,
        regions: [Vec<(u64, u64)>; 3],
        sketch: FrequencySketch,
        rng: u64,
        hasher: Fixed,
    }

    impl Reference {
        fn new(capacity: u64) -> Self {
            let hasher = fixed();
            Reference {
                capacity,
                regions: Default::default(),
                sketch: FrequencySketch::new(0),
                rng: hasher.hash_one(0x7419_1fe5_u64),
                hasher,
            }
        }

        fn hash(&self, k: u64) -> u64 {
            self.hasher.hash_one(k)
        }

        fn len(&self) -> usize {
            self.regions.iter().map(Vec::len).sum()
        }

        fn bytes(&self) -> u64 {
            self.regions.iter().flatten().map(|o| o.1).sum()
        }

        fn region_bytes(&self, r: Region) -> u64 {
            self.regions[r as usize].iter().map(|o| o.1).sum()
        }

        fn find(&self, k: u64) -> Option<(Region, usize)> {
            [Region::Window, Region::Probation, Region::Protected]
                .into_iter()
                .find_map(|r| {
                    self.regions[r as usize]
                        .iter()
                        .position(|o| o.0 == k)
                        .map(|p| (r, p))
                })
        }

        fn take(&mut self, (r, p): (Region, usize)) -> (u64, u64) {
            self.regions[r as usize].remove(p)
        }

        fn demote(&mut self) {
            let (_, protected) = shares(self.capacity);
            while self.region_bytes(Region::Protected) > protected {
                let o = self.regions[Region::Protected as usize].remove(0);
                self.regions[Region::Probation as usize].push(o);
            }
        }

        fn hit(&mut self, at: (Region, usize)) {
            let o = self.take(at);
            self.sketch.increment(self.hash(o.0));
            match at.0 {
                Region::Probation => {
                    self.regions[Region::Protected as usize].push(o);
                    self.demote();
                }
                r => self.regions[r as usize].push(o),
            }
        }

        fn get(&mut self, k: u64) -> bool {
            match self.find(k) {
                Some(at) => {
                    self.hit(at);
                    true
                }
                None => false,
            }
        }

        fn admit(&mut self, c: u8, v: u8) -> bool {
            if c > v {
                return true;
            }
            if c < HASH_FLOOD_THRESHOLD {
                return false;
            }
            self.rng = self.rng.wrapping_add(GOLDEN);
            spread(self.rng) & 127 == 0
        }

        fn evict_to(&mut self, limit: u64, out: &mut Vec<(u64, u64)>) {
            let (window, _) = shares(self.capacity);
            // The candidates are the last `fresh` objects of probation, in
            // the order they left the window.
            let mut fresh = 0;
            while self.region_bytes(Region::Window) > window {
                let o = self.regions[Region::Window as usize].remove(0);
                self.regions[Region::Probation as usize].push(o);
                fresh += 1;
            }
            while self.bytes() > limit {
                let probation = &self.regions[Region::Probation as usize];
                let first = probation.len() - fresh.min(probation.len());
                if fresh > 0 && first > 0 {
                    let c = self.sketch.frequency(self.hash(probation[first].0));
                    let v = self.sketch.frequency(self.hash(probation[0].0));
                    let p = if self.admit(c, v) { 0 } else { first };
                    fresh -= 1;
                    out.push(self.take((Region::Probation, p)));
                } else {
                    fresh = 0;
                    let r = [Region::Probation, Region::Protected, Region::Window]
                        .into_iter()
                        .find(|&r| !self.regions[r as usize].is_empty())
                        .unwrap();
                    out.push(self.take((r, 0)));
                }
            }
        }

        fn insert(&mut self, k: u64, size: u64, out: &mut Vec<(u64, u64)>) -> Inserted {
            if let Some(at) = self.find(k) {
                if size > self.capacity {
                    out.push(self.take(at));
                    return Inserted::TooLarge;
                }
                self.regions[at.0 as usize][at.1].1 = size;
                self.hit(at);
                self.demote();
                self.evict_to(self.capacity, out);
                return if self.find(k).is_some() {
                    Inserted::Replaced
                } else {
                    Inserted::Rejected
                };
            }
            if size > self.capacity {
                return Inserted::TooLarge;
            }
            self.regions[Region::Window as usize].push((k, size));
            self.sketch.ensure_capacity(self.len());
            self.sketch.increment(self.hash(k));
            self.evict_to(self.capacity, out);
            if self.find(k).is_some() {
                Inserted::Stored
            } else {
                Inserted::Rejected
            }
        }

        fn set_capacity(&mut self, c: u64, out: &mut Vec<(u64, u64)>) {
            self.capacity = c;
            self.demote();
            self.evict_to(c, out);
        }
    }

    #[derive(Debug, Clone)]
    enum Op {
        Get(u64),
        Insert(u64, u64),
        Remove(u64),
        Resize(u64),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            10 => (0u64..64).prop_map(|k| Op::Get(k * k / 64)),
            8 => (0u64..64, 1u64..90).prop_map(|(k, s)| Op::Insert(k, s)),
            1 => (0u64..64).prop_map(Op::Remove),
            1 => (0u64..3_000).prop_map(Op::Resize),
        ]
    }

    fn check(t: &TinyLfu<u64, Fixed>, r: &Reference) -> Result<(), TestCaseError> {
        let regions = t.regions();
        prop_assert_eq!(&regions, &r.regions);
        prop_assert_eq!(t.bytes(), r.bytes());
        prop_assert!(t.bytes() <= t.capacity());
        let (_, protected) = shares(t.capacity());
        prop_assert!(t.region_bytes(Region::Protected) <= protected);
        prop_assert_eq!(t.sketch.words(), r.sketch.words());
        prop_assert_eq!(t.sketch.resets(), r.sketch.resets());
        prop_assert_eq!(t.candidate, NIL);
        prop_assert_eq!(t.watch, NIL);
        Ok(())
    }

    /// A sketch with exact counts beside it, saturating and halved at the
    /// same moments.
    fn exact_beside(sketch: &mut FrequencySketch, exact: &mut HashMap<u64, u8>, key: u64) {
        let resets = sketch.resets();
        sketch.increment(key);
        let c = exact.entry(key).or_default();
        *c = (*c + 1).min(15);
        if sketch.resets() != resets {
            for c in exact.values_mut() {
                *c /= 2;
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Every operation leaves the same regions, in the same order, as
        /// the reference, and evicts the same keys in the same order.
        #[test]
        fn matches_the_reference(
            capacity in 1u64..3_000,
            ops in proptest::collection::vec(op(), 1..600),
        ) {
            let mut t = TinyLfu::with_hasher(capacity, fixed());
            let mut r = Reference::new(capacity);
            for op in ops {
                let mut got = Vec::new();
                let mut want = Vec::new();
                match op {
                    Op::Get(k) => prop_assert_eq!(t.get(&k), r.get(k)),
                    Op::Insert(k, s) => {
                        let did = t.insert(k, s, |k, s| got.push((k, s)));
                        prop_assert_eq!(did, r.insert(k, s, &mut want));
                    }
                    Op::Remove(k) => {
                        let at = r.find(k);
                        prop_assert_eq!(t.remove(&k), at.map(|at| r.take(at).1));
                    }
                    Op::Resize(c) => {
                        t.set_capacity(c, |k, s| got.push((k, s)));
                        r.set_capacity(c, &mut want);
                    }
                }
                prop_assert_eq!(got, want);
                check(&t, &r)?;
            }
        }

        /// The simulator's pattern (look up, insert on a miss) over a
        /// skewed trace long enough to age and grow the sketch.
        #[test]
        fn matches_the_reference_on_a_trace(
            capacity in 1u64..20_000,
            trace in proptest::collection::vec((0u64..2_000, 1u64..120), 1..3_000),
        ) {
            let mut t = TinyLfu::with_hasher(capacity, fixed());
            let mut r = Reference::new(capacity);
            for (id, size) in trace {
                let id = id * id / 2_000;
                let size = size.min(1 + id * 7);
                let hit = t.get(&id);
                prop_assert_eq!(hit, r.get(id));
                if !hit {
                    let mut want = Vec::new();
                    prop_assert_eq!(t.insert(id, size, |_, _| ()), r.insert(id, size, &mut want));
                }
            }
            check(&t, &r)?;
        }

        /// The sketch never estimates below a key's true count, saturated
        /// at 15 and halved when the sketch halves.
        #[test]
        fn the_sketch_never_undercounts(
            keys in proptest::collection::vec(0u64..300, 1..4_000),
        ) {
            let mut sketch = FrequencySketch::new(0);
            let mut exact = HashMap::new();
            for k in keys {
                exact_beside(&mut sketch, &mut exact, k);
            }
            for (k, c) in exact {
                prop_assert!(sketch.frequency(k) >= c, "key {} estimated {} under {}", k, sketch.frequency(k), c);
            }
        }
    }

    #[test]
    fn the_sketch_ages_and_stays_close() {
        // 64 words: halving after 640 increments.
        let mut sketch = FrequencySketch::new(0);
        assert_eq!(sketch.words(), 64);
        for _ in 0..20 {
            sketch.increment(7);
        }
        assert_eq!(sketch.frequency(7), 15, "saturates");
        assert!(!sketch.increment(7), "nothing rose");
        let mut exact = HashMap::new();
        for i in 0..639u64 {
            exact_beside(&mut sketch, &mut exact, 1_000 + i % 50);
        }
        assert_eq!(sketch.resets(), 1, "halved once");
        assert_eq!(sketch.frequency(7), 7, "15 halved");
        // With 51 keys over 1,024 counters per row, most estimates are
        // exact and none is far off.
        let over: u32 = exact
            .iter()
            .map(|(&k, &c)| u32::from(sketch.frequency(k) - c))
            .sum();
        assert!(over <= 10, "overestimated by {over} in all");
        // Growing starts again from zero and keeps the reset count.
        sketch.ensure_capacity(64);
        assert_eq!(sketch.words(), 64, "already big enough");
        sketch.ensure_capacity(100);
        assert_eq!((sketch.words(), sketch.resets()), (128, 1));
        assert_eq!(sketch.frequency(7), 0);
        assert_eq!(FrequencySketch::new(usize::MAX).words(), MAX_WORDS);
    }

    #[test]
    fn shares_of_the_budget() {
        assert_eq!(shares(0), (0, 0));
        assert_eq!(shares(99), (0, 79));
        assert_eq!(shares(1_000), (10, 792));
        assert_eq!(shares(12_345), (123, 9_777));
        let (w, p) = shares(u64::MAX);
        assert_eq!(w, u64::MAX / 100);
        assert!(p > u64::MAX / 2 && p < u64::MAX - w);
    }

    fn keys(t: &TinyLfu<u64, Fixed>) -> [Vec<u64>; 3] {
        t.regions().map(|r| r.into_iter().map(|o| o.0).collect())
    }

    #[test]
    fn a_worked_example() {
        // 1,000 bytes: a 10-byte window, 792 bytes of protected. Objects of
        // 100 bytes are larger than the window, so each is a candidate as
        // soon as it is written.
        let mut t = TinyLfu::with_hasher(1_000, fixed());
        let none = |_: u64, _: u64| panic!("room for all");
        for k in 1..=10 {
            assert_eq!(t.insert(k, 100, none), Inserted::Stored);
        }
        assert_eq!(keys(&t), [vec![], (1..=10).collect(), vec![]]);
        assert!(t.get(&3) && t.get(&5));
        assert_eq!(t.region_of(&3), Some(Region::Protected));
        assert_eq!(keys(&t)[2], [3, 5]);
        // 11 duels 1, probation's oldest. Both were seen once, and a tie
        // keeps the victim, so 11 goes straight back.
        let mut out = Vec::new();
        assert_eq!(t.insert(11, 100, |k, _| out.push(k)), Inserted::Rejected);
        assert_eq!(out, [11]);
        assert!(!t.contains(&11));
        // Seen a second time, 11 is estimated above 1 and takes its place.
        out.clear();
        assert_eq!(t.frequency(&11), 1);
        assert_eq!(t.insert(11, 100, |k, _| out.push(k)), Inserted::Stored);
        assert_eq!(out, [1]);
        assert_eq!(
            keys(&t),
            [vec![], vec![2, 4, 6, 7, 8, 9, 10, 11], vec![3, 5]]
        );
        // A small object fits the window: it is not a candidate yet, so
        // room is made in plain LRU order, from probation.
        out.clear();
        assert_eq!(t.insert(12, 5, |k, _| out.push(k)), Inserted::Stored);
        assert_eq!(out, [2]);
        assert_eq!(t.region_of(&12), Some(Region::Window));
        // A second one pushes the window past its 10 bytes: 12 becomes a
        // candidate, but the cache is within budget, so nothing goes.
        assert_eq!(t.insert(13, 6, none), Inserted::Stored);
        assert_eq!(keys(&t)[0], [13]);
        assert_eq!(t.region_of(&12), Some(Region::Probation));
        assert_eq!(t.bytes(), 911);
    }

    #[test]
    fn protected_overflow_goes_back_to_probation() {
        let mut t = TinyLfu::with_hasher(1_000, fixed());
        for k in 1..=9 {
            t.insert(k, 100, |_, _| ());
        }
        // Protected holds 792 bytes: seven of these.
        for k in 1..=8 {
            t.get(&k);
        }
        assert_eq!(keys(&t)[2], (2..=8).collect::<Vec<_>>());
        assert_eq!(t.region_of(&1), Some(Region::Probation));
        // Read again in protected, 2 moves to its most recent end.
        t.get(&2);
        assert_eq!(keys(&t)[2], [3, 4, 5, 6, 7, 8, 2]);
    }

    #[test]
    fn hot_objects_survive_a_scan() {
        let mut t = TinyLfu::new(100);
        for k in 0..10u64 {
            for _ in 0..3 {
                if !t.get(&k) {
                    t.insert(k, 1, |_, _| ());
                }
            }
        }
        for k in 1_000..20_000u64 {
            for h in 0..10 {
                assert!(t.get(&h), "hot {h} evicted at scan {k}");
            }
            if !t.get(&k) {
                t.insert(k, 1, |_, _| ());
            }
        }
        assert_eq!(t.bytes(), 100);
    }

    #[test]
    fn a_new_hot_set_displaces_an_old_one_as_the_sketch_ages() {
        // Ten objects of 10 bytes fill 100 bytes; the old set is read until
        // it saturates, then never again, and a new set takes over while a
        // stream of one-hit objects keeps the sketch counting (a saturated
        // counter does not count toward aging).
        let mut t = TinyLfu::with_hasher(100, fixed());
        let access = |t: &mut TinyLfu<u64, Fixed>, k: u64| {
            if !t.get(&k) {
                t.insert(k, 10, |_, _| ());
            }
        };
        for _ in 0..20 {
            for k in 0..9 {
                access(&mut t, k);
            }
        }
        for round in 0..2_000 {
            for k in 100..109 {
                access(&mut t, k);
            }
            access(&mut t, 1_000_000 + round);
        }
        assert!(t.sketch.resets() > 0);
        for k in 100..109 {
            assert!(t.contains(&k), "new hot {k} admitted");
        }
        for k in 0..9 {
            assert!(!t.contains(&k), "old {k} aged out");
        }
    }

    #[test]
    fn a_flooded_victim_is_still_displaced_at_random() {
        // The victim is saturated; a candidate at 6 or more still gets in
        // one time in 128 or so.
        let mut t: TinyLfu<u64, Fixed> = TinyLfu::with_hasher(100, fixed());
        let mut admitted = 0;
        for _ in 0..12_800 {
            if t.admit(HASH_FLOOD_THRESHOLD, 15) {
                admitted += 1;
            }
            assert!(!t.admit(HASH_FLOOD_THRESHOLD - 1, 15));
            assert!(t.admit(3, 2));
        }
        assert!((50..200).contains(&admitted), "{admitted} of 12,800");
    }

    #[test]
    fn replace_resize_remove_and_refusal() {
        let mut t: TinyLfu<String> = TinyLfu::new(300);
        let none = |_: String, _: u64| panic!("nothing evicted");
        assert_eq!(t.insert("a".into(), 100, none), Inserted::Stored);
        assert_eq!(t.insert("b".into(), 100, none), Inserted::Stored);
        assert_eq!(t.insert("c".into(), 301, none), Inserted::TooLarge);
        assert!(!t.contains("c"));
        assert_eq!(t.region_of("a"), Some(Region::Probation));
        // Growing a held object counts as a read and evicts others.
        let mut out = Vec::new();
        assert_eq!(
            t.insert("b".into(), 250, |k, _| out.push(k)),
            Inserted::Replaced
        );
        assert_eq!(out, ["a"]);
        assert_eq!((t.bytes(), t.size_of("b")), (250, Some(250)));
        assert_eq!(
            t.region_of("b"),
            Some(Region::Probation),
            "too big for protected"
        );
        // A replacement too large for the cache drops the old copy.
        out.clear();
        assert_eq!(
            t.insert("b".into(), 400, |k, _| out.push(k)),
            Inserted::TooLarge
        );
        assert_eq!(out, ["b"]);
        assert!(t.is_empty());
        assert_eq!(t.remove("b"), None);
        for k in ["x", "y", "z"] {
            t.insert(k.into(), 100, none);
        }
        assert_eq!(t.remove("y"), Some(100));
        assert_eq!(t.size_of("y"), None);
        assert_eq!(t.region_of("y"), None);
        out.clear();
        t.set_capacity(100, |k, _| out.push(k));
        assert_eq!(out, ["x"]);
        assert_eq!((t.len(), t.capacity()), (1, 100));
        t.set_capacity(0, |k, _| out.push(k));
        assert!(t.is_empty());
        assert_eq!(t.bytes(), 0);
    }

    #[test]
    fn the_sketch_grows_with_the_objects_held() {
        let mut t = TinyLfu::new(1 << 20);
        for k in 0..1_000u64 {
            t.insert(k, 1, |_, _| ());
        }
        assert_eq!(t.sketch.words(), 1_024);
    }
}
