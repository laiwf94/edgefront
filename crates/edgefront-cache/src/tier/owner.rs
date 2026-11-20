//! Which node owns a cache key: weighted rendezvous hashing (Thaler and
//! Ravishankar, 1998; the weighted form of Schindelhauer and Schomaker,
//! 2005).
//!
//! Every node scores every key, `weight / -ln(h)` with `h` a hash of the
//! key and the node mapped into (0, 1), and the highest score owns it. A
//! node's share of keys is its weight over the total, and when a node joins
//! or leaves, only the keys it gains or loses move: every other key keeps
//! its owner, since the other nodes' scores for it do not change. The hash
//! is fixed here (FNV-1a and a SplitMix64 finaliser), not the standard
//! library's, so every build in a cluster agrees on owners.

/// One node that can own keys.
#[derive(Debug, Clone, PartialEq)]
pub struct Node<T> {
    pub id: String,
    /// Relative share of keys: a weight of 2 owns twice the keys of 1.
    pub weight: f64,
    /// What the caller needs to reach the node.
    pub value: T,
}

/// The nodes keys are spread over.
#[derive(Debug, Clone)]
pub struct Owners<T> {
    nodes: Vec<(u64, Node<T>)>,
}

/// FNV-1a over `bytes`, 64-bit.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The SplitMix64 finaliser: every input bit moves every output bit.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl<T> Owners<T> {
    /// Nodes with a weight that is not positive and finite own nothing and
    /// are left out.
    pub fn new(nodes: impl IntoIterator<Item = Node<T>>) -> Self {
        let mut nodes: Vec<(u64, Node<T>)> = nodes
            .into_iter()
            .filter(|n| n.weight.is_finite() && n.weight > 0.0)
            .map(|n| (fnv1a(n.id.as_bytes()), n))
            .collect();
        nodes.sort_by(|a, b| a.1.id.cmp(&b.1.id));
        nodes.dedup_by(|a, b| a.1.id == b.1.id);
        Owners { nodes }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn nodes(&self) -> impl Iterator<Item = &Node<T>> {
        self.nodes.iter().map(|(_, n)| n)
    }

    fn score(key: u64, node: u64, weight: f64) -> f64 {
        // 53 bits into (0, 1): never 0, so the logarithm is finite and
        // negative.
        let h = mix(key ^ mix(node));
        let unit = ((h >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
        weight / -unit.ln()
    }

    /// The owner of `key`.
    pub fn owner(&self, key: &str) -> Option<&Node<T>> {
        let k = fnv1a(key.as_bytes());
        self.nodes
            .iter()
            .map(|(h, n)| (Self::score(k, *h, n.weight), n))
            .max_by(|a, b| a.0.total_cmp(&b.0).then_with(|| b.1.id.cmp(&a.1.id)))
            .map(|(_, n)| n)
    }

    /// The `n` highest-scoring nodes for `key`, the owner first: where the
    /// key goes next if the owner leaves.
    pub fn top(&self, key: &str, n: usize) -> Vec<&Node<T>> {
        let k = fnv1a(key.as_bytes());
        let mut scored: Vec<(f64, &Node<T>)> = self
            .nodes
            .iter()
            .map(|(h, node)| (Self::score(k, *h, node.weight), node))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
        scored.into_iter().take(n).map(|(_, node)| node).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn nodes(n: usize) -> Vec<Node<()>> {
        (0..n)
            .map(|i| Node {
                id: format!("edge-{i}"),
                weight: 1.0,
                value: (),
            })
            .collect()
    }

    fn keys(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("GET example.com /asset/{i}.js"))
            .collect()
    }

    #[test]
    fn ten_equal_nodes_share_keys_evenly() {
        let o = Owners::new(nodes(10));
        let mut count = std::collections::BTreeMap::new();
        for k in keys(100_000) {
            *count.entry(o.owner(&k).unwrap().id.clone()).or_insert(0) += 1;
        }
        for (id, n) in &count {
            assert!((9_000..=11_000).contains(n), "{id} owns {n} of 100,000");
        }
    }

    #[test]
    fn removing_one_of_ten_moves_only_its_keys() {
        let all = Owners::new(nodes(10));
        let rest = Owners::new(nodes(10).into_iter().filter(|n| n.id != "edge-3"));
        let ks = keys(100_000);
        let mut moved = 0;
        for k in &ks {
            let before = &all.owner(k).unwrap().id;
            let after = &rest.owner(k).unwrap().id;
            if before != after {
                assert_eq!(before, "edge-3", "{k} moved from {before}");
                moved += 1;
            }
        }
        // Its own share, about a tenth, and the done-when's bound of 11%.
        println!("{moved} of 100,000 keys moved");
        assert!(moved < 11_000, "{moved} of 100,000 moved");
        assert!(moved > 9_000, "{moved} of 100,000 moved");
    }

    #[test]
    fn weights_set_the_share() {
        let mut ns = nodes(3);
        ns[0].weight = 2.0;
        let o = Owners::new(ns);
        let mut heavy = 0;
        for k in keys(40_000) {
            if o.owner(&k).unwrap().id == "edge-0" {
                heavy += 1;
            }
        }
        // 2 / (2 + 1 + 1) = half.
        assert!((19_000..=21_000).contains(&heavy), "{heavy}");
    }

    #[test]
    fn empty_bad_weights_duplicates_and_the_runners_up() {
        let none: Owners<()> = Owners::new([]);
        assert!(none.owner("k").is_none() && none.is_empty());
        let mut ns = nodes(4);
        ns[1].weight = 0.0;
        ns[2].weight = f64::NAN;
        ns.push(ns[0].clone());
        let o = Owners::new(ns);
        assert_eq!(o.len(), 2);
        assert_eq!(o.nodes().count(), 2);
        let o = Owners::new(nodes(5));
        let top = o.top("some key", 3);
        assert_eq!(top.len(), 3);
        assert_eq!(top[0].id, o.owner("some key").unwrap().id);
        assert!(top[0].id != top[1].id && top[1].id != top[2].id);
        assert_eq!(o.top("some key", 9).len(), 5);
        // Stable across calls and builds: a fixed key has a fixed owner.
        assert_eq!(
            Owners::new(nodes(10))
                .owner("GET example.com /")
                .unwrap()
                .id,
            Owners::new(nodes(10))
                .owner("GET example.com /")
                .unwrap()
                .id
        );
    }

    proptest! {
        #[test]
        fn a_membership_change_moves_only_the_changed_nodes_keys(
            n in 2usize..16,
            gone in 0usize..16,
            weights in proptest::collection::vec(1u8..5, 16),
            ks in proptest::collection::vec("[a-z0-9/]{1,24}", 1..200),
        ) {
            let gone = gone % n;
            let all: Vec<Node<()>> = (0..n)
                .map(|i| Node { id: format!("n{i}"), weight: f64::from(weights[i]), value: () })
                .collect();
            let before = Owners::new(all.clone());
            let gone_id = format!("n{gone}");
            let after = Owners::new(all.into_iter().filter(|x| x.id != gone_id));
            for k in &ks {
                let b = &before.owner(k).unwrap().id;
                let a = &after.owner(k).unwrap().id;
                // Leaving moves only the leaver's keys; joining (the same
                // change read backwards) moves keys only to the joiner.
                prop_assert!(b == a || *b == gone_id, "{} moved {} -> {}", k, b, a);
                // A moved key goes to the runner-up it had before.
                if b != a {
                    prop_assert_eq!(&before.top(k, 2)[1].id, a);
                }
            }
        }
    }
}
