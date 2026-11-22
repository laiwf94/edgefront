//! Least recently used, the baseline every eviction study reports.
//!
//! An intrusive doubly linked list over a slab of nodes, indexed by id, so
//! a hit and an eviction are O(1) with no allocation once the slab is warm.

use super::{IdMap, Policy};

pub const NAME: &str = "lru";

const NIL: u32 = u32::MAX;

struct Node {
    id: u64,
    size: u64,
    prev: u32,
    next: u32,
}

pub struct Lru {
    capacity: u64,
    bytes: u64,
    map: IdMap<u32>,
    nodes: Vec<Node>,
    free: Vec<u32>,
    /// Most recently used.
    head: u32,
    /// Least recently used, the next victim.
    tail: u32,
}

impl Lru {
    pub fn new(capacity: u64) -> Self {
        Lru {
            capacity,
            bytes: 0,
            map: IdMap::default(),
            nodes: Vec::new(),
            free: Vec::new(),
            head: NIL,
            tail: NIL,
        }
    }

    fn unlink(&mut self, i: u32) {
        let (prev, next) = {
            let n = &self.nodes[i as usize];
            (n.prev, n.next)
        };
        match prev {
            NIL => self.head = next,
            p => self.nodes[p as usize].next = next,
        }
        match next {
            NIL => self.tail = prev,
            n => self.nodes[n as usize].prev = prev,
        }
    }

    fn push_front(&mut self, i: u32) {
        let old = self.head;
        {
            let n = &mut self.nodes[i as usize];
            n.prev = NIL;
            n.next = old;
        }
        match old {
            NIL => self.tail = i,
            o => self.nodes[o as usize].prev = i,
        }
        self.head = i;
    }

    /// Ids from most to least recently used, for tests.
    #[cfg(test)]
    fn order(&self) -> Vec<u64> {
        let mut out = Vec::new();
        let mut i = self.head;
        while i != NIL {
            out.push(self.nodes[i as usize].id);
            i = self.nodes[i as usize].next;
        }
        out
    }
}

impl Policy for Lru {
    fn name(&self) -> &'static str {
        NAME
    }

    fn get(&mut self, id: u64) -> bool {
        let Some(&i) = self.map.get(&id) else {
            return false;
        };
        if self.head != i {
            self.unlink(i);
            self.push_front(i);
        }
        true
    }

    fn insert(&mut self, id: u64, size: u64) -> bool {
        if size > self.capacity || self.map.contains_key(&id) {
            return false;
        }
        while self.bytes + size > self.capacity {
            let victim = self.tail;
            self.unlink(victim);
            let node = &self.nodes[victim as usize];
            self.bytes -= node.size;
            self.map.remove(&node.id);
            self.free.push(victim);
        }
        let node = Node {
            id,
            size,
            prev: NIL,
            next: NIL,
        };
        let i = match self.free.pop() {
            Some(i) => {
                self.nodes[i as usize] = node;
                i
            }
            None => {
                let i = u32::try_from(self.nodes.len()).expect("fewer than 2^32 objects held");
                self.nodes.push(node);
                i
            }
        };
        self.push_front(i);
        self.map.insert(id, i);
        self.bytes += size;
        true
    }

    fn bytes(&self) -> u64 {
        self.bytes
    }

    fn len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// LRU as a list, most recent first: the obvious O(n) version.
    struct Reference {
        capacity: u64,
        list: Vec<(u64, u64)>,
    }

    impl Reference {
        fn get(&mut self, id: u64) -> bool {
            match self.list.iter().position(|&(i, _)| i == id) {
                Some(p) => {
                    let item = self.list.remove(p);
                    self.list.insert(0, item);
                    true
                }
                None => false,
            }
        }

        fn insert(&mut self, id: u64, size: u64) -> bool {
            if size > self.capacity {
                return false;
            }
            while self.list.iter().map(|&(_, s)| s).sum::<u64>() + size > self.capacity {
                self.list.pop();
            }
            self.list.insert(0, (id, size));
            true
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn matches_the_reference(
            capacity in 1u64..400,
            trace in proptest::collection::vec((0u64..60, 1u64..80), 1..800),
        ) {
            let mut lru = Lru::new(capacity);
            let mut reference = Reference { capacity, list: Vec::new() };
            for (id, size) in trace {
                // Each id keeps one size, as in a trace of unchanging objects.
                let size = size.min(1 + id * 3);
                let hit = lru.get(id);
                prop_assert_eq!(hit, reference.get(id));
                if !hit {
                    prop_assert_eq!(lru.insert(id, size), reference.insert(id, size));
                }
                let order: Vec<u64> = reference.list.iter().map(|&(i, _)| i).collect();
                prop_assert_eq!(lru.order(), order);
                prop_assert_eq!(lru.bytes(), reference.list.iter().map(|&(_, s)| s).sum::<u64>());
                prop_assert_eq!(lru.len(), reference.list.len());
            }
        }
    }

    #[test]
    fn evicts_the_least_recently_used_and_reuses_its_node() {
        let mut lru = Lru::new(30);
        for id in 1..=3 {
            assert!(lru.insert(id, 10));
        }
        assert!(lru.get(1));
        assert!(lru.insert(4, 10));
        assert_eq!(lru.order(), vec![4, 1, 3]);
        assert_eq!(lru.nodes.len(), 3, "the evicted node is reused");
        assert!(!lru.insert(4, 10), "a held id is a hit, not an insert");
        assert!(!lru.insert(5, 31));
        assert!(lru.insert(6, 30));
        assert_eq!(lru.order(), vec![6]);
        assert!(lru.get(6));
        assert_eq!(lru.order(), vec![6]);
    }
}
