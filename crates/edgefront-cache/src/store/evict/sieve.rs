//! SIEVE eviction (Zhang et al., "SIEVE is Simpler than LRU", NSDI 2024),
//! over byte sizes.
//!
//! Objects sit in one FIFO queue, newest at the head, each with a visited
//! bit. A hit sets the bit and moves nothing, so it needs only a shared
//! reference: the bit is an atomic, and a store can count reads under a
//! read lock as the memory tier does. To make room, the hand starts where
//! it last stopped (the tail, the oldest object, when it has no place) and
//! walks toward the head, clearing set bits, wrapping to the tail at the
//! head, until it finds an object whose bit is clear. That object is
//! evicted and the hand stays at the next newer one. New objects are
//! inserted at the head and wait there for the hand, so one-hit objects
//! leave on its next pass while objects read since its last pass survive
//!
//! The queue is an intrusive doubly linked list over a dense slab of
//! nodes, indexed by a hash map from key to slot, so a hit, an insert and
//! an eviction are O(1) amortised, with no allocation once the slab is
//! warm. A removed node's slot is filled by moving the last node into it,
//! so the slab never holds dead keys.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash};
use std::sync::atomic::{AtomicBool, Ordering};

const NIL: u32 = u32::MAX;

struct Node<K> {
    key: K,
    size: u64,
    /// Toward the head: the next newer object, the way the hand moves.
    newer: u32,
    /// Toward the tail: the next older object.
    older: u32,
    visited: AtomicBool,
}

/// What [`Sieve::insert`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inserted {
    /// The key was new and is now held, at the head.
    Stored,
    /// The key was held: its size is updated in place and the write counts
    /// as a read, so it keeps its place and its bit is set.
    Replaced,
    /// The object is larger than the whole capacity and was not stored. A
    /// replaced key whose new size is too large is removed.
    TooLarge,
}

/// A SIEVE queue holding up to `capacity` bytes.
pub struct Sieve<K, S = RandomState> {
    capacity: u64,
    bytes: u64,
    map: HashMap<K, u32, S>,
    nodes: Vec<Node<K>>,
    /// Newest object.
    head: u32,
    /// Oldest object.
    tail: u32,
    /// The next object the hand examines; NIL starts it at the tail.
    hand: u32,
}

impl<K: Hash + Eq + Clone> Sieve<K> {
    pub fn new(capacity: u64) -> Self {
        Self::with_hasher(capacity, RandomState::new())
    }
}

impl<K: Hash + Eq + Clone, S: BuildHasher> Sieve<K, S> {
    pub fn with_hasher(capacity: u64, hasher: S) -> Self {
        Sieve {
            capacity,
            bytes: 0,
            map: HashMap::with_hasher(hasher),
            nodes: Vec::new(),
            head: NIL,
            tail: NIL,
            hand: NIL,
        }
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Bytes held.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Objects held.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Looks `key` up and counts the read by setting its visited bit. True
    /// when it is held. Takes a shared reference, so concurrent readers
    /// need no exclusive lock.
    pub fn get<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match self.map.get(key) {
            Some(&i) => {
                let visited = &self.nodes[i as usize].visited;
                // Skipping the store when the bit is already set keeps the
                // cache line shared between readers of a hot object.
                if !visited.load(Ordering::Relaxed) {
                    visited.store(true, Ordering::Relaxed);
                }
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

    /// The size `key` is held at.
    pub fn size_of<Q>(&self, key: &Q) -> Option<u64>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.get(key).map(|&i| self.nodes[i as usize].size)
    }

    /// Stores `key` at `size` bytes, evicting until it fits and handing
    /// each evicted key and its size to `evicted`, oldest decision first.
    pub fn insert(&mut self, key: K, size: u64, mut evicted: impl FnMut(K, u64)) -> Inserted {
        if let Some(&i) = self.map.get(&key) {
            if size > self.capacity {
                let (key, old) = self.remove_at(i);
                evicted(key, old);
                return Inserted::TooLarge;
            }
            let node = &mut self.nodes[i as usize];
            self.bytes = self.bytes - node.size + size;
            node.size = size;
            *node.visited.get_mut() = true;
            self.evict_to(self.capacity, &mut evicted);
            return Inserted::Replaced;
        }
        if size > self.capacity {
            return Inserted::TooLarge;
        }
        self.evict_to(self.capacity - size, &mut evicted);
        let i = u32::try_from(self.nodes.len())
            .ok()
            .filter(|&i| i != NIL)
            .expect("fewer than 2^32 - 1 objects held");
        let old_head = self.head;
        self.nodes.push(Node {
            key: key.clone(),
            size,
            newer: NIL,
            older: old_head,
            visited: AtomicBool::new(false),
        });
        match old_head {
            NIL => self.tail = i,
            h => self.nodes[h as usize].newer = i,
        }
        self.head = i;
        self.map.insert(key, i);
        self.bytes += size;
        Inserted::Stored
    }

    /// Removes `key`, as a purge does, returning the size it was held at.
    /// The hand moves on to the next newer object if it was on this one.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<u64>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let &i = self.map.get(key)?;
        Some(self.remove_at(i).1)
    }

    /// Changes the byte budget, evicting down to a smaller one.
    pub fn set_capacity(&mut self, capacity: u64, mut evicted: impl FnMut(K, u64)) {
        self.capacity = capacity;
        self.evict_to(capacity, &mut evicted);
    }

    /// Runs the hand until at most `limit` bytes are held.
    fn evict_to(&mut self, limit: u64, evicted: &mut impl FnMut(K, u64)) {
        while self.bytes > limit {
            let victim = self.find_victim();
            // Removing the node the hand is on leaves it on the next newer.
            self.hand = victim;
            let (key, size) = self.remove_at(victim);
            evicted(key, size);
        }
    }

    /// The first object from the hand toward the head, wrapping, whose bit
    /// is clear, clearing the bits it passes. The queue is not empty.
    fn find_victim(&mut self) -> u32 {
        let mut i = if self.hand == NIL {
            self.tail
        } else {
            self.hand
        };
        loop {
            let node = &mut self.nodes[i as usize];
            let visited = node.visited.get_mut();
            if !*visited {
                return i;
            }
            *visited = false;
            i = if node.newer == NIL {
                self.tail
            } else {
                node.newer
            };
        }
    }

    /// Unlinks slot `i` and fills it with the last node.
    fn remove_at(&mut self, i: u32) -> (K, u64) {
        let (newer, older) = {
            let n = &self.nodes[i as usize];
            (n.newer, n.older)
        };
        match newer {
            NIL => self.head = older,
            n => self.nodes[n as usize].older = older,
        }
        match older {
            NIL => self.tail = newer,
            o => self.nodes[o as usize].newer = newer,
        }
        if self.hand == i {
            self.hand = newer;
        }
        let last = u32::try_from(self.nodes.len() - 1).expect("slots fit in u32");
        let node = self.nodes.swap_remove(i as usize);
        self.map.remove(&node.key);
        if i != last {
            // The node that was last now lives in slot i: repoint its
            // neighbours, the ends, the hand and its map entry.
            let (newer, older) = {
                let n = &self.nodes[i as usize];
                (n.newer, n.older)
            };
            match newer {
                NIL => self.head = i,
                n => self.nodes[n as usize].older = i,
            }
            match older {
                NIL => self.tail = i,
                o => self.nodes[o as usize].newer = i,
            }
            if self.hand == last {
                self.hand = i;
            }
            *self
                .map
                .get_mut(&self.nodes[i as usize].key)
                .expect("every node is mapped") = i;
        }
        self.bytes -= node.size;
        (node.key, node.size)
    }

    /// The keys from the tail (oldest) to the head, each with its size and
    /// visited bit, and the key the hand is on.
    #[cfg(test)]
    fn queue(&self) -> (Vec<(K, u64, bool)>, Option<K>) {
        let mut out = Vec::new();
        let mut i = self.tail;
        let mut prev = NIL;
        while i != NIL {
            let n = &self.nodes[i as usize];
            assert_eq!(n.older, prev, "links agree");
            assert_eq!(self.map[&n.key], i, "the map points at the node");
            out.push((n.key.clone(), n.size, n.visited.load(Ordering::Relaxed)));
            prev = i;
            i = n.newer;
        }
        assert_eq!(prev, self.head);
        assert_eq!(out.len(), self.nodes.len());
        assert_eq!(out.len(), self.map.len());
        let hand = (self.hand != NIL).then(|| self.nodes[self.hand as usize].key.clone());
        (out, hand)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// SIEVE as the paper's pseudocode over a vector, oldest first: the
    /// obvious O(n) version, with the hand as an index.
    #[derive(Default)]
    struct Reference {
        capacity: u64,
        /// (key, size, visited), oldest first.
        queue: Vec<(u64, u64, bool)>,
        hand: Option<usize>,
    }

    impl Reference {
        fn bytes(&self) -> u64 {
            self.queue.iter().map(|o| o.1).sum()
        }

        fn find(&self, key: u64) -> Option<usize> {
            self.queue.iter().position(|o| o.0 == key)
        }

        fn get(&mut self, key: u64) -> bool {
            match self.find(key) {
                Some(p) => {
                    self.queue[p].2 = true;
                    true
                }
                None => false,
            }
        }

        fn remove(&mut self, p: usize) -> (u64, u64) {
            let (key, size, _) = self.queue.remove(p);
            self.hand = match self.hand {
                // The hand stays on the next newer object, which has
                // shifted down into p; past the head it has no place.
                Some(h) if h == p => (p < self.queue.len()).then_some(p),
                Some(h) if h > p => Some(h - 1),
                h => h,
            };
            (key, size)
        }

        fn evict_to(&mut self, limit: u64, out: &mut Vec<(u64, u64)>) {
            while self.bytes() > limit {
                let mut p = self.hand.unwrap_or(0);
                while self.queue[p].2 {
                    self.queue[p].2 = false;
                    p = (p + 1) % self.queue.len();
                }
                self.hand = Some(p);
                out.push(self.remove(p));
            }
        }

        fn insert(&mut self, key: u64, size: u64, out: &mut Vec<(u64, u64)>) -> Inserted {
            if let Some(p) = self.find(key) {
                if size > self.capacity {
                    out.push(self.remove(p));
                    return Inserted::TooLarge;
                }
                self.queue[p].1 = size;
                self.queue[p].2 = true;
                self.evict_to(self.capacity, out);
                return Inserted::Replaced;
            }
            if size > self.capacity {
                return Inserted::TooLarge;
            }
            self.evict_to(self.capacity - size, out);
            self.queue.push((key, size, false));
            Inserted::Stored
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
            8 => (0u64..48).prop_map(Op::Get),
            8 => (0u64..48, 1u64..90).prop_map(|(k, s)| Op::Insert(k, s)),
            1 => (0u64..48).prop_map(Op::Remove),
            1 => (0u64..500).prop_map(Op::Resize),
        ]
    }

    fn check(sieve: &Sieve<u64>, reference: &Reference) -> Result<(), TestCaseError> {
        let (queue, hand) = sieve.queue();
        prop_assert_eq!(&queue, &reference.queue);
        prop_assert_eq!(hand, reference.hand.map(|h| reference.queue[h].0));
        prop_assert_eq!(sieve.bytes(), reference.bytes());
        prop_assert!(sieve.bytes() <= sieve.capacity());
        prop_assert_eq!(sieve.len(), reference.queue.len());
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Every operation leaves the same queue, bits, hand and bytes as
        /// the reference, and evicts the same keys in the same order.
        #[test]
        fn matches_the_reference(
            capacity in 1u64..400,
            ops in proptest::collection::vec(op(), 1..600),
        ) {
            let mut sieve = Sieve::new(capacity);
            let mut reference = Reference { capacity, ..Reference::default() };
            for op in ops {
                let mut got = Vec::new();
                let mut want = Vec::new();
                match op {
                    Op::Get(k) => prop_assert_eq!(sieve.get(&k), reference.get(k)),
                    Op::Insert(k, s) => {
                        let did = sieve.insert(k, s, |k, s| got.push((k, s)));
                        prop_assert_eq!(did, reference.insert(k, s, &mut want));
                    }
                    Op::Remove(k) => {
                        let p = reference.find(k);
                        prop_assert_eq!(sieve.remove(&k), p.map(|p| reference.remove(p).1));
                    }
                    Op::Resize(c) => {
                        sieve.set_capacity(c, |k, s| got.push((k, s)));
                        reference.capacity = c;
                        reference.evict_to(c, &mut want);
                    }
                }
                prop_assert_eq!(got, want);
                check(&sieve, &reference)?;
            }
        }

        /// The simulator's access pattern (look up, insert on a miss) over
        /// a skewed trace, where most of the hand's work happens.
        #[test]
        fn matches_the_reference_on_a_trace(
            capacity in 1u64..2_000,
            trace in proptest::collection::vec((0u64..400, 1u64..120), 1..2_000),
        ) {
            let mut sieve = Sieve::new(capacity);
            let mut reference = Reference { capacity, ..Reference::default() };
            for (id, size) in trace {
                // Skewed toward low ids, each with one size.
                let id = id * id / 400;
                let size = size.min(1 + id * 7);
                let hit = sieve.get(&id);
                prop_assert_eq!(hit, reference.get(id));
                if !hit {
                    let mut want = Vec::new();
                    prop_assert_eq!(sieve.insert(id, size, |_, _| ()), reference.insert(id, size, &mut want));
                }
            }
            check(&sieve, &reference)?;
        }
    }

    fn keys(s: &Sieve<u64>) -> Vec<u64> {
        s.queue().0.into_iter().map(|o| o.0).collect()
    }

    #[test]
    fn a_worked_example() {
        // 1 to 8 inserted oldest first, with 1, 2 and 7 read. The hand
        // starts at the tail: 1 and 2 are spared and cleared, 3 goes, and
        // the hand stays on 4.
        let mut s = Sieve::new(8);
        for k in 1..=8 {
            assert_eq!(
                s.insert(k, 1, |_, _| panic!("room for all")),
                Inserted::Stored
            );
        }
        for k in [1, 2, 7] {
            assert!(s.get(&k));
        }
        let mut out = Vec::new();
        assert_eq!(s.insert(9, 1, |k, _| out.push(k)), Inserted::Stored);
        assert_eq!(out, [3]);
        assert_eq!(s.queue().1, Some(4));
        assert_eq!(keys(&s), [1, 2, 4, 5, 6, 7, 8, 9]);
        // Four bytes of room: 4, 5 and 6 go, 7 is spared and cleared, 8
        // goes, and the hand stops on 9 with its bit still set.
        assert!(s.get(&9));
        out.clear();
        s.insert(10, 4, |k, _| out.push(k));
        assert_eq!(out, [4, 5, 6, 8]);
        assert_eq!(keys(&s), [1, 2, 7, 9, 10]);
        let (q, hand) = s.queue();
        assert_eq!(hand, Some(9));
        assert!(
            q.iter().find(|o| o.0 == 7).is_some_and(|o| !o.2),
            "7's bit was cleared"
        );
        assert!(
            q.iter().find(|o| o.0 == 9).is_some_and(|o| o.2),
            "9 was not reached"
        );
    }

    #[test]
    fn hot_objects_survive_a_scan() {
        let mut s = Sieve::new(100);
        for k in 0..10 {
            s.insert(k, 1, |_, _| ());
        }
        for k in 1_000..10_000 {
            // The hot set is read between scan objects, as live traffic is.
            for h in 0..10 {
                assert!(s.get(&h), "hot {h} evicted at scan {k}");
            }
            if !s.get(&k) {
                s.insert(k, 1, |_, _| ());
            }
        }
        assert_eq!(s.bytes(), 100);
    }

    #[test]
    fn hand_wraps_and_clears_every_bit_when_all_were_read() {
        let mut s = Sieve::new(3);
        for k in 1..=3 {
            s.insert(k, 1, |_, _| ());
            s.get(&k);
        }
        let mut out = Vec::new();
        s.insert(4, 1, |k, _| out.push(k));
        // A full lap clears every bit, then the oldest goes.
        assert_eq!(out, [1]);
        assert_eq!(s.queue().0, [(2, 1, false), (3, 1, false), (4, 1, false)]);
        assert_eq!(s.queue().1, Some(2));
    }

    #[test]
    fn replace_resize_remove_and_refusal() {
        let mut s: Sieve<String> = Sieve::new(30);
        let none = |_: String, _: u64| panic!("nothing evicted");
        assert_eq!(s.insert("a".into(), 10, none), Inserted::Stored);
        assert_eq!(s.insert("b".into(), 10, none), Inserted::Stored);
        assert_eq!(s.insert("c".into(), 31, none), Inserted::TooLarge);
        assert!(!s.contains("c"));
        // Growing a held object evicts others, never the object itself.
        let mut out = Vec::new();
        assert_eq!(
            s.insert("b".into(), 25, |k, _| out.push(k)),
            Inserted::Replaced
        );
        assert_eq!(out, ["a"]);
        assert_eq!((s.bytes(), s.size_of("b")), (25, Some(25)));
        assert!(s.contains("b") && !s.get("a"));
        // A replacement too large for the cache drops the old copy.
        out.clear();
        assert_eq!(
            s.insert("b".into(), 40, |k, _| out.push(k)),
            Inserted::TooLarge
        );
        assert_eq!(out, ["b"]);
        assert!(s.is_empty());
        assert_eq!(s.bytes(), 0);
        assert_eq!(s.remove("b"), None);
        for k in ["x", "y", "z"] {
            s.insert(k.into(), 10, none);
        }
        assert_eq!(s.remove("y"), Some(10));
        assert_eq!(s.size_of("y"), None);
        out.clear();
        s.set_capacity(10, |k, _| out.push(k));
        assert_eq!(out, ["x"]);
        assert_eq!((s.len(), s.capacity()), (1, 10));
        s.set_capacity(0, |k, _| out.push(k));
        assert!(s.is_empty());
    }

    #[test]
    fn readers_share_it_across_threads() {
        let mut s = Sieve::new(1_000);
        for k in 0..1_000u64 {
            s.insert(k, 1, |_, _| ());
        }
        std::thread::scope(|scope| {
            for t in 0..4u64 {
                let s = &s;
                scope.spawn(move || {
                    for k in (t..1_000).step_by(8) {
                        assert!(s.get(&k));
                    }
                });
            }
        });
        let (q, _) = s.queue();
        for (k, _, visited) in q {
            assert_eq!(visited, k % 8 < 4, "key {k}");
        }
    }
}
