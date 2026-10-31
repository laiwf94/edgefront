//! Longest-prefix lookup over byte strings with a compressed (radix) trie.
//!
//! steer kept each virtual host's routes sorted longest prefix first and
//! tried them in turn, which costs one comparison per route. The trie walks
//! the path once instead, so a lookup costs at most one step per edge on the
//! path and does not grow with the number of routes.

/// An immutable map from byte-string prefixes to values, answering "which
/// stored prefix of this key is the longest". Built once, then only read.
#[derive(Debug, Clone)]
pub(crate) struct PrefixTree<V> {
    root: Node<V>,
}

#[derive(Debug, Clone)]
struct Node<V> {
    /// The bytes on the edge from the parent to this node.
    label: Box<[u8]>,
    value: Option<V>,
    /// First byte of each child's label, sorted, parallel to `children`.
    firsts: Vec<u8>,
    children: Vec<Node<V>>,
}

impl<V> Node<V> {
    fn new(label: &[u8], value: Option<V>) -> Self {
        Self {
            label: label.into(),
            value,
            firsts: Vec::new(),
            children: Vec::new(),
        }
    }
}

impl<V> Default for PrefixTree<V> {
    fn default() -> Self {
        Self {
            root: Node::new(b"", None),
        }
    }
}

impl<V> PrefixTree<V> {
    /// Stores `value` under `prefix`. The first value stored under a prefix
    /// wins, matching steer, where a stable sort kept the earlier of two
    /// routes with the same host and prefix in front.
    pub(crate) fn insert(&mut self, prefix: &[u8], value: V) {
        let mut node = &mut self.root;
        let mut rest = prefix;
        loop {
            let Some(&first) = rest.first() else {
                if node.value.is_none() {
                    node.value = Some(value);
                }
                return;
            };
            let at = match node.firsts.binary_search(&first) {
                Err(at) => {
                    node.firsts.insert(at, first);
                    node.children.insert(at, Node::new(rest, Some(value)));
                    return;
                }
                Ok(at) => at,
            };
            let child = &mut node.children[at];
            let common = common_prefix_len(&child.label, rest);
            if common < child.label.len() {
                // Split the edge: an intermediate node takes the shared part
                // and the old child keeps the remainder below it.
                let mut lower = std::mem::replace(child, Node::new(&rest[..common], None));
                lower.label = lower.label[common..].into();
                child.firsts.push(lower.label[0]);
                child.children.push(lower);
            }
            node = &mut node.children[at];
            rest = &rest[common..];
        }
    }

    /// The value stored under the longest prefix of `key`, if any.
    pub(crate) fn longest_prefix(&self, key: &[u8]) -> Option<&V> {
        let mut node = &self.root;
        let mut rest = key;
        let mut best = node.value.as_ref();
        while let Some(&first) = rest.first() {
            let Ok(at) = node.firsts.binary_search(&first) else {
                break;
            };
            let child = &node.children[at];
            let Some(after) = rest.strip_prefix(&*child.label) else {
                break;
            };
            if child.value.is_some() {
                best = child.value.as_ref();
            }
            node = child;
            rest = after;
        }
        best
    }
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(entries: &[(&str, u32)]) -> PrefixTree<u32> {
        let mut t = PrefixTree::default();
        for &(p, v) in entries {
            t.insert(p.as_bytes(), v);
        }
        t
    }

    #[test]
    fn longest_stored_prefix_wins() {
        let t = tree(&[("/", 1), ("/api/", 2), ("/api/v2/", 3), ("/apiary", 4)]);
        assert_eq!(t.longest_prefix(b"/"), Some(&1));
        assert_eq!(t.longest_prefix(b"/api"), Some(&1));
        assert_eq!(t.longest_prefix(b"/api/"), Some(&2));
        assert_eq!(t.longest_prefix(b"/api/v1/x"), Some(&2));
        assert_eq!(t.longest_prefix(b"/api/v2/x"), Some(&3));
        assert_eq!(t.longest_prefix(b"/apiary/bees"), Some(&4));
        assert_eq!(t.longest_prefix(b"/apia"), Some(&1));
        assert_eq!(t.longest_prefix(b"x"), None);
        assert_eq!(t.longest_prefix(b""), None);
    }

    #[test]
    fn empty_prefix_matches_everything() {
        let t = tree(&[("", 7), ("/a", 8)]);
        assert_eq!(t.longest_prefix(b""), Some(&7));
        assert_eq!(t.longest_prefix(b"zzz"), Some(&7));
        assert_eq!(t.longest_prefix(b"/ab"), Some(&8));
    }

    #[test]
    fn first_insert_wins_for_duplicates() {
        let t = tree(&[("/x", 1), ("/x", 2), ("", 3), ("", 4)]);
        assert_eq!(t.longest_prefix(b"/x"), Some(&1));
        assert_eq!(t.longest_prefix(b"/"), Some(&3));
    }

    #[test]
    fn splits_edges_in_any_insert_order() {
        // Insert the long key first so the shorter ones split its edge,
        // including a split that lands on an existing branch point.
        let t = tree(&[
            ("/abcdef", 1),
            ("/abc", 2),
            ("/abx", 3),
            ("/ab", 4),
            ("/b", 5),
        ]);
        assert_eq!(t.longest_prefix(b"/abcdefg"), Some(&1));
        assert_eq!(t.longest_prefix(b"/abcde"), Some(&2));
        assert_eq!(t.longest_prefix(b"/abxy"), Some(&3));
        assert_eq!(t.longest_prefix(b"/abz"), Some(&4));
        assert_eq!(t.longest_prefix(b"/a"), None);
        assert_eq!(t.longest_prefix(b"/b/c"), Some(&5));
    }

    #[test]
    fn works_on_arbitrary_bytes() {
        let t = tree(&[("é", 1), ("\u{e9}x", 2)]);
        // "é" is two bytes; a key sharing only the first byte matches nothing.
        assert_eq!(t.longest_prefix(&[0xc3]), None);
        assert_eq!(t.longest_prefix("éy".as_bytes()), Some(&1));
        assert_eq!(t.longest_prefix("éx".as_bytes()), Some(&2));
    }
}
