//! A list that is cheap to copy and cheap to change by a little.
//!
//! [`Chunked`] keeps its items in chunks of [`CHUNK`], every chunk full but
//! the last, behind reference counts. A copy shares every chunk, and a
//! change copies only what it touches: the list of chunk pointers (one
//! per [`CHUNK`] items) and the one or two chunks the change lands in. So
//! a list of 100,000 items is copied in O(1) and changed by one item in
//! about 800 pointer copies plus a chunk or two of 128 items, where a
//! `Vec` would copy all 100,000. Removal is by swap, as [`Vec::swap_remove`]: the last item
//! takes the removed one's place, so indices move only for that one item.
//!
//! The proxy keeps a cluster's endpoints this way, so a discovery change
//! to a cluster of 100,000 endpoints is a patch of a few items while the
//! requests in flight keep the list they started with.

use std::fmt;
use std::sync::Arc;

/// Items per chunk: a power of two, so an index splits with a shift and a
/// mask.
pub const CHUNK: usize = 128;
const SHIFT: u32 = CHUNK.trailing_zeros();
const MASK: usize = CHUNK - 1;

/// A persistent list in chunks of [`CHUNK`] items; see the module.
pub struct Chunked<T> {
    chunks: Arc<Vec<Arc<Vec<T>>>>,
    len: usize,
}

impl<T> Chunked<T> {
    /// An empty list.
    pub fn new() -> Chunked<T> {
        Chunked {
            chunks: Arc::new(Vec::new()),
            len: 0,
        }
    }

    /// How many items it holds.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether it holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The item at `index`, if there is one.
    pub fn get(&self, index: usize) -> Option<&T> {
        if index >= self.len {
            return None;
        }
        Some(&self.chunks[index >> SHIFT][index & MASK])
    }

    /// The last item, if there is one.
    pub fn last(&self) -> Option<&T> {
        self.len.checked_sub(1).and_then(|i| self.get(i))
    }

    /// The items in order.
    pub fn iter(&self) -> Iter<'_, T> {
        Iter {
            chunks: self.chunks.iter(),
            current: [].iter(),
            left: self.len,
        }
    }

    /// Whether `self` and `other` share every chunk, so neither has been
    /// changed since one was copied from the other.
    pub fn ptr_eq(&self, other: &Chunked<T>) -> bool {
        Arc::ptr_eq(&self.chunks, &other.chunks)
    }
}

impl<T: Clone> Chunked<T> {
    /// The chunk at `chunk`, copied first if another list shares it.
    fn chunk_mut(&mut self, chunk: usize) -> &mut Vec<T> {
        Arc::make_mut(&mut Arc::make_mut(&mut self.chunks)[chunk])
    }

    /// Adds `item` at the end, at index `len()` before the call.
    pub fn push(&mut self, item: T) {
        if self.len & MASK == 0 {
            let mut chunk = Vec::with_capacity(CHUNK);
            chunk.push(item);
            Arc::make_mut(&mut self.chunks).push(Arc::new(chunk));
        } else {
            let last = self.len >> SHIFT;
            self.chunk_mut(last).push(item);
        }
        self.len += 1;
    }

    /// Removes and returns the last item.
    pub fn pop(&mut self) -> Option<T> {
        let i = self.len.checked_sub(1)?;
        let chunk = i >> SHIFT;
        let item = self.chunk_mut(chunk).pop();
        if i & MASK == 0 {
            Arc::make_mut(&mut self.chunks).pop();
        }
        self.len = i;
        item
    }

    /// Replaces the item at `index` with `item` and returns the old one.
    ///
    /// # Panics
    ///
    /// If `index` is out of bounds.
    pub fn set(&mut self, index: usize, item: T) -> T {
        assert!(index < self.len, "index {index} of {}", self.len);
        std::mem::replace(&mut self.chunk_mut(index >> SHIFT)[index & MASK], item)
    }

    /// Removes the item at `index` and returns it, moving the last item
    /// into its place, as [`Vec::swap_remove`] does.
    ///
    /// # Panics
    ///
    /// If `index` is out of bounds.
    pub fn swap_remove(&mut self, index: usize) -> T {
        assert!(index < self.len, "index {index} of {}", self.len);
        let last = self.pop().expect("not empty");
        if index == self.len {
            last
        } else {
            self.set(index, last)
        }
    }

    /// The items in order, copied into a `Vec`.
    pub fn to_vec(&self) -> Vec<T> {
        self.iter().cloned().collect()
    }
}

impl<T> Default for Chunked<T> {
    fn default() -> Self {
        Chunked::new()
    }
}

impl<T> Clone for Chunked<T> {
    /// Shares every chunk: O(1).
    fn clone(&self) -> Self {
        Chunked {
            chunks: Arc::clone(&self.chunks),
            len: self.len,
        }
    }
}

impl<T> std::ops::Index<usize> for Chunked<T> {
    type Output = T;

    fn index(&self, index: usize) -> &T {
        match self.get(index) {
            Some(item) => item,
            None => panic!("index {index} of {}", self.len),
        }
    }
}

impl<T> FromIterator<T> for Chunked<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut chunks = Vec::new();
        let mut chunk = Vec::with_capacity(CHUNK);
        let mut len = 0;
        for item in iter {
            chunk.push(item);
            len += 1;
            if chunk.len() == CHUNK {
                chunks.push(Arc::new(std::mem::replace(
                    &mut chunk,
                    Vec::with_capacity(CHUNK),
                )));
            }
        }
        if !chunk.is_empty() {
            chunks.push(Arc::new(chunk));
        }
        Chunked {
            chunks: Arc::new(chunks),
            len,
        }
    }
}

impl<T> From<Vec<T>> for Chunked<T> {
    fn from(items: Vec<T>) -> Self {
        items.into_iter().collect()
    }
}

impl<T: PartialEq> PartialEq for Chunked<T> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

impl<T: Eq> Eq for Chunked<T> {}

impl<T: PartialEq> PartialEq<[T]> for Chunked<T> {
    fn eq(&self, other: &[T]) -> bool {
        self.len == other.len() && self.iter().eq(other.iter())
    }
}

impl<T: fmt::Debug> fmt::Debug for Chunked<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<'a, T> IntoIterator for &'a Chunked<T> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;

    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}

/// The items of a [`Chunked`], in order.
#[derive(Debug, Clone)]
pub struct Iter<'a, T> {
    chunks: std::slice::Iter<'a, Arc<Vec<T>>>,
    current: std::slice::Iter<'a, T>,
    left: usize,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        loop {
            if let Some(item) = self.current.next() {
                self.left -= 1;
                return Some(item);
            }
            self.current = self.chunks.next()?.iter();
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl<T> ExactSizeIterator for Iter<'_, T> {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A list and a `Vec` given the same operations stay equal, and the
    /// chunk invariant (every chunk full but the last, none empty) holds.
    fn check(list: &Chunked<u32>, model: &[u32]) {
        assert_eq!(list.len(), model.len());
        assert_eq!(list.to_vec(), model);
        assert_eq!(list.iter().len(), model.len());
        let chunks = &list.chunks;
        assert_eq!(chunks.len(), model.len().div_ceil(CHUNK));
        for (i, c) in chunks.iter().enumerate() {
            if i + 1 < chunks.len() {
                assert_eq!(c.len(), CHUNK);
            } else {
                assert!(!c.is_empty());
            }
        }
        for (i, v) in model.iter().enumerate() {
            assert_eq!(list[i], *v);
        }
        assert_eq!(list.get(model.len()), None);
        assert_eq!(list.last(), model.last());
    }

    #[test]
    fn behaves_like_a_vec() {
        let mut list = Chunked::new();
        let mut model = Vec::new();
        check(&list, &model);
        // A deterministic mix of pushes, sets, pops and swap removals over
        // several chunk boundaries.
        let mut x: u64 = 7;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for step in 0..20_000u32 {
            let r = next();
            match r % 10 {
                0..=5 => {
                    list.push(step);
                    model.push(step);
                }
                6 if !model.is_empty() => {
                    let i = (next() as usize) % model.len();
                    assert_eq!(list.swap_remove(i), model.swap_remove(i));
                }
                7 if !model.is_empty() => {
                    assert_eq!(list.pop(), model.pop());
                }
                8 if !model.is_empty() => {
                    let i = (next() as usize) % model.len();
                    let old = model[i];
                    model[i] = step;
                    assert_eq!(list.set(i, step), old);
                }
                _ => {}
            }
            if step % 997 == 0 {
                check(&list, &model);
            }
        }
        check(&list, &model);
        while let Some(v) = model.pop() {
            assert_eq!(list.pop(), Some(v));
        }
        check(&list, &model);
        assert_eq!(list.pop(), None);
    }

    #[test]
    fn a_copy_is_untouched_by_changes_to_the_original() {
        let mut list: Chunked<u32> = (0..CHUNK as u32 * 4 - 48).collect();
        let copy = list.clone();
        assert!(list.ptr_eq(&copy));
        let n = CHUNK as u32 * 4 - 48;
        list.swap_remove(3);
        list.set(CHUNK * 2 + 5, 9);
        list.push(77);
        assert!(!list.ptr_eq(&copy));
        assert_eq!(copy.to_vec(), (0..n).collect::<Vec<_>>());
        assert_eq!(list[3], n - 1);
        assert_eq!(list[CHUNK * 2 + 5], 9);
        assert_eq!(list.last(), Some(&77));
        // Only the chunks a change touched were copied: 0 (the removal),
        // 2 (the set) and 3 (the pop that fed the removal, then the push).
        let shared = |i: usize| Arc::ptr_eq(&list.chunks[i], &copy.chunks[i]);
        assert!(!shared(0));
        assert!(shared(1));
        assert!(!shared(2));
        assert!(!shared(3));
    }

    #[test]
    fn conversions_equality_and_debug() {
        let list: Chunked<u32> = vec![1, 2, 3].into();
        assert_eq!(list, Chunked::from(vec![1, 2, 3]));
        assert_ne!(list, Chunked::from(vec![1, 2]));
        assert_ne!(list, Chunked::from(vec![1, 2, 4]));
        assert!(list == *[1, 2, 3].as_slice());
        assert_eq!(format!("{list:?}"), "[1, 2, 3]");
        assert_eq!((&list).into_iter().copied().sum::<u32>(), 6);
        assert!(Chunked::<u32>::default().is_empty());
        let exact: Chunked<usize> = (0..CHUNK).collect();
        assert_eq!(exact.chunks.len(), 1);
    }

    #[test]
    #[should_panic(expected = "index 3 of 3")]
    fn index_out_of_bounds_panics() {
        let list: Chunked<u32> = vec![1, 2, 3].into();
        let _ = list[3];
    }

    #[test]
    #[should_panic(expected = "index 0 of 0")]
    fn swap_remove_out_of_bounds_panics() {
        Chunked::<u32>::new().swap_remove(0);
    }
}
