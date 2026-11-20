//! Prefix lists: sets of IPv4 and IPv6 prefixes, each with a value, and
//! longest-prefix match on an address.
//!
//! A [`PrefixList`] keeps one path-compressed binary radix tree per
//! address family. A node stands for one prefix (its bits and length) and
//! has at most two children, the prefixes that continue with a 0 and with
//! a 1; a node that only joins two branches carries no value. Chains of
//! single children are compressed away, so a list of `n` prefixes has at
//! most `2n` nodes, and a lookup compares the address with at most one
//! node per bit where the stored prefixes branch: under 33 nodes for IPv4
//! and 129 for IPv6, usually far fewer. Nodes live in one vector and refer
//! to each other by index, so a tree is two allocations and no pointers.
//!
//! An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) is looked up as the IPv4
//! address, as the limiter keys clients ([`crate::conn::ClientKey`]).
//!
//! Lists are built off the data path ([`PrefixList::parse`] or
//! [`PrefixList::insert`]) and published with [`Swap`]: readers keep their
//! own handle ([`Reader`]) that checks one shared generation number per
//! lookup and takes the new list only when it changed, so a reload never
//! blocks a lookup and lookups write no shared memory.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// No node or no value.
const NONE: u32 = u32::MAX;

/// The bits of an address, most significant first: `u32` for IPv4 and
/// `u128` for IPv6.
trait Bits: Copy + Eq + Ord + fmt::Debug {
    const WIDTH: u8;
    fn zero() -> Self;
    /// Bits `skip..skip + bits` (from the most significant) as a number;
    /// `skip + bits <= WIDTH` and `bits >= 1`.
    fn window(self, skip: u8, bits: u8) -> usize;
    /// `self`'s first `skip` bits, then `b` in the next `bits`, zeros after.
    fn with_window(self, skip: u8, bits: u8, b: usize) -> Self;
    fn to_ip(self) -> IpAddr;
    /// The last address of the prefix `self/len`.
    fn last(self, len: u8) -> Self;
    /// The next address, or `None` past the last.
    fn succ(self) -> Option<Self>;
    /// Bit `i`, counted from the most significant (0).
    fn bit(self, i: u8) -> usize;
    /// The first `len` bits, the rest zero.
    fn masked(self, len: u8) -> Self;
    /// How many leading bits `self` and `other` share.
    fn common(self, other: Self) -> u8;
}

impl Bits for u32 {
    const WIDTH: u8 = 32;
    fn zero() -> Self {
        0
    }
    fn window(self, skip: u8, bits: u8) -> usize {
        ((self << skip) >> (32 - bits)) as usize
    }
    fn with_window(self, skip: u8, bits: u8, b: usize) -> Self {
        self.masked(skip) | ((b as u32) << (32 - skip - bits))
    }
    fn to_ip(self) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(self))
    }
    fn last(self, len: u8) -> Self {
        self | !(u32::MAX.masked(len))
    }
    fn succ(self) -> Option<Self> {
        self.checked_add(1)
    }
    fn bit(self, i: u8) -> usize {
        ((self >> (31 - u32::from(i))) & 1) as usize
    }
    fn masked(self, len: u8) -> Self {
        if len == 0 {
            0
        } else {
            self & (u32::MAX << (32 - u32::from(len)))
        }
    }
    fn common(self, other: Self) -> u8 {
        (self ^ other).leading_zeros() as u8
    }
}

impl Bits for u128 {
    const WIDTH: u8 = 128;
    fn zero() -> Self {
        0
    }
    fn window(self, skip: u8, bits: u8) -> usize {
        ((self << skip) >> (128 - bits)) as usize
    }
    fn with_window(self, skip: u8, bits: u8, b: usize) -> Self {
        self.masked(skip) | ((b as u128) << (128 - skip - bits))
    }
    fn to_ip(self) -> IpAddr {
        IpAddr::V6(Ipv6Addr::from(self))
    }
    fn last(self, len: u8) -> Self {
        self | !(u128::MAX.masked(len))
    }
    fn succ(self) -> Option<Self> {
        self.checked_add(1)
    }
    fn bit(self, i: u8) -> usize {
        ((self >> (127 - u32::from(i))) & 1) as usize
    }
    fn masked(self, len: u8) -> Self {
        if len == 0 {
            0
        } else {
            self & (u128::MAX << (128 - u32::from(len)))
        }
    }
    fn common(self, other: Self) -> u8 {
        (self ^ other).leading_zeros() as u8
    }
}

#[derive(Clone, Copy, Debug)]
struct Node<K> {
    key: K,
    len: u8,
    value: u32,
    child: [u32; 2],
}

/// One family's tree. Node 0 is the root, the zero-length prefix.
#[derive(Clone, Debug)]
struct Tree<K> {
    nodes: Vec<Node<K>>,
}

impl<K: Bits> Tree<K> {
    fn new() -> Self {
        Self {
            nodes: vec![Node {
                key: K::zero(),
                len: 0,
                value: NONE,
                child: [NONE; 2],
            }],
        }
    }

    fn push(&mut self, key: K, len: u8, value: u32) -> u32 {
        self.nodes.push(Node {
            key,
            len,
            value,
            child: [NONE; 2],
        });
        (self.nodes.len() - 1) as u32
    }

    /// Sets the value of prefix `key/len` (`key` already masked) to
    /// `value`, returning the value it replaced.
    fn insert(&mut self, key: K, len: u8, value: u32) -> Option<u32> {
        let mut at = 0u32;
        loop {
            let n = self.nodes[at as usize];
            if n.len == len {
                let old = n.value;
                self.nodes[at as usize].value = value;
                return (old != NONE).then_some(old);
            }
            let b = key.bit(n.len);
            let c = n.child[b];
            if c == NONE {
                let leaf = self.push(key, len, value);
                self.nodes[at as usize].child[b] = leaf;
                return None;
            }
            let cn = self.nodes[c as usize];
            let shared = key.common(cn.key).min(len).min(cn.len);
            if shared == cn.len {
                at = c;
                continue;
            }
            // Split: a node for the shared prefix takes the child's place.
            let mid = if shared == len {
                self.push(key, len, value)
            } else {
                self.push(key.masked(shared), shared, NONE)
            };
            self.nodes[mid as usize].child[cn.key.bit(shared)] = c;
            if shared != len {
                let leaf = self.push(key, len, value);
                self.nodes[mid as usize].child[key.bit(shared)] = leaf;
            }
            self.nodes[at as usize].child[b] = mid;
            return None;
        }
    }

    /// The value of the longest stored prefix of `addr`, and its length.
    #[inline]
    fn lookup(&self, addr: K) -> Option<(u32, u8)> {
        let nodes = &self.nodes[..];
        let mut n = &nodes[0];
        let mut best = (n.value != NONE).then_some((n.value, 0));
        while n.len < K::WIDTH {
            let c = n.child[addr.bit(n.len)];
            if c == NONE {
                break;
            }
            n = &nodes[c as usize];
            if addr.masked(n.len) != n.key {
                break;
            }
            if n.value != NONE {
                best = Some((n.value, n.len));
            }
        }
        best
    }

    /// The value stored for exactly `key/len`.
    fn exact(&self, key: K, len: u8) -> Option<u32> {
        let mut n = &self.nodes[0];
        loop {
            if n.len == len {
                return (n.key == key && n.value != NONE).then_some(n.value);
            }
            let c = n.child[key.bit(n.len)];
            if c == NONE {
                return None;
            }
            n = &self.nodes[c as usize];
            if n.len > len || key.masked(n.len) != n.key {
                return None;
            }
        }
    }
}

/// One range of the compiled index: from `start` to the next range's
/// start, the longest prefix covering it is value `idx`, of length `len`
/// (`idx == NONE`: none). Keeping the length here means a lookup builds
/// its [`Match`] without reading the prefix array.
#[derive(Clone, Copy, Debug)]
struct Range<K> {
    start: K,
    idx: u32,
    len: u8,
}

/// The most bucket bits: a table of 2^20 + 1 `u32`s, 4 MiB.
const MAX_BUCKET_BITS: u8 = 20;

/// The compiled form of one family's prefixes: the address space cut
/// into ranges sorted by start, each with the longest prefix covering it,
/// and a bucket table over the bits just past the ones every range
/// boundary shares (so a list inside 2001:db8::/32 still spreads over
/// every bucket). A lookup reads one bucket entry and binary-searches the
/// handful of ranges of that bucket, often within one cache line, where
/// walking the tree reads one node per level.
#[derive(Clone, Debug)]
struct Ranges<K> {
    ranges: Vec<Range<K>>,
    /// The bits every boundary from `ranges[1]` on shares, then how many
    /// bits after them pick a bucket (0: no table, at most 2 ranges).
    skip: u8,
    bits: u8,
    /// `bucket[b]` is the index of the range holding bucket `b`'s first
    /// address; the bucket's ranges run up to `bucket[b + 1]` inclusive.
    bucket: Vec<u32>,
}

impl<K: Bits> Ranges<K> {
    /// From `(key, len, value)` prefixes, keys masked, values below
    /// `lens.len()` with `lens[value]` the value's prefix length.
    fn build(mut prefixes: Vec<(K, u8, u32)>) -> Self {
        // Outer prefixes before the ones they contain.
        prefixes.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let mut out: Vec<Range<K>> = vec![Range {
            start: K::zero(),
            idx: NONE,
            len: 0,
        }];
        let emit = |out: &mut Vec<Range<K>>, start: K, idx: u32, len: u8| {
            let last = out.last_mut().expect("one range at least");
            if last.start == start {
                (last.idx, last.len) = (idx, len);
            } else {
                out.push(Range { start, idx, len });
            }
        };
        // The prefixes containing the current position, innermost last:
        // (last address, value, length).
        let mut stack: Vec<(K, u32, u8)> = Vec::new();
        let close = |out: &mut Vec<Range<K>>, stack: &mut Vec<(K, u32, u8)>| {
            let (end, _, _) = stack.pop().expect("an open prefix");
            if let Some(next) = end.succ() {
                let (idx, len) = stack.last().map_or((NONE, 0), |&(_, v, l)| (v, l));
                emit(out, next, idx, len);
            }
        };
        for (key, len, v) in prefixes {
            while stack.last().is_some_and(|&(end, _, _)| end < key) {
                close(&mut out, &mut stack);
            }
            emit(&mut out, key, v, len);
            stack.push((key.last(len), v, len));
        }
        while !stack.is_empty() {
            close(&mut out, &mut stack);
        }
        // Merge neighbours that ended up with one value.
        let mut ranges: Vec<Range<K>> = Vec::with_capacity(out.len());
        for r in out {
            if ranges.last().is_none_or(|l| l.idx != r.idx) {
                ranges.push(r);
            }
        }
        ranges.shrink_to_fit();
        let n = ranges.len();
        if n < 3 {
            return Self {
                ranges,
                skip: 0,
                bits: 0,
                bucket: Vec::new(),
            };
        }
        let (lo, hi) = (ranges[1].start, ranges[n - 1].start);
        let skip = lo.common(hi);
        let want = (usize::BITS - (n - 1).leading_zeros()) as u8;
        let bits = want.clamp(1, MAX_BUCKET_BITS).min(K::WIDTH - skip);
        let count = 1usize << bits;
        let mut bucket = Vec::with_capacity(count + 1);
        let mut i = 0usize;
        for b in 0..count {
            let first = lo.with_window(skip, bits, b);
            while i + 1 < n && ranges[i + 1].start <= first {
                i += 1;
            }
            bucket.push(i as u32);
        }
        bucket.push((n - 1) as u32);
        Self {
            ranges,
            skip,
            bits,
            bucket,
        }
    }

    /// The range holding `addr`.
    #[inline]
    fn lookup(&self, addr: K) -> &Range<K> {
        let r = &self.ranges;
        let n = r.len();
        if n == 1 || addr < r[1].start {
            return &r[0];
        }
        if addr >= r[n - 1].start {
            return &r[n - 1];
        }
        // Between the first and last boundaries, so `addr` shares their
        // first `skip` bits and the bucket is its next `bits`.
        let b = addr.window(self.skip, self.bits);
        let mut lo = self.bucket[b] as usize;
        let mut len = self.bucket[b + 1] as usize + 1 - lo;
        // The last start <= addr in r[lo..lo + len].
        while len > 1 {
            let half = len / 2;
            if r[lo + half].start <= addr {
                lo += half;
            }
            len -= half;
        }
        &r[lo]
    }

    fn memory(&self) -> usize {
        self.ranges.capacity() * std::mem::size_of::<Range<K>>() + self.bucket.capacity() * 4
    }
}

/// An IPv4 or IPv6 prefix: an address with its host bits clear, and a
/// length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Prefix {
    addr: IpAddr,
    len: u8,
}

/// Why a prefix or a list did not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixError(pub String);

impl fmt::Display for PrefixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PrefixError {}

/// An IPv4-mapped IPv6 address as its IPv4 address; every other address
/// as it is.
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

impl Prefix {
    /// The prefix `addr/len`. Refused when `len` is past the family's
    /// width or `addr` has bits set past `len`; an IPv4-mapped IPv6 prefix
    /// of `/96` or longer is its IPv4 prefix.
    pub fn new(addr: IpAddr, len: u8) -> Result<Self, PrefixError> {
        let (addr, len) = match addr {
            IpAddr::V6(v6) if len >= 96 && v6.to_ipv4_mapped().is_some() => (
                IpAddr::V4(v6.to_ipv4_mapped().unwrap_or(Ipv4Addr::UNSPECIFIED)),
                len - 96,
            ),
            a => (a, len),
        };
        let ok = match addr {
            IpAddr::V4(v4) => len <= 32 && u32::from(v4).masked(len) == u32::from(v4),
            IpAddr::V6(v6) => len <= 128 && u128::from(v6).masked(len) == u128::from(v6),
        };
        if !ok {
            let width = if addr.is_ipv4() { 32 } else { 128 };
            return Err(PrefixError(if len > width {
                format!("{addr}/{len}: the length is over {width}")
            } else {
                format!("{addr}/{len}: host bits are set past /{len}")
            }));
        }
        Ok(Self { addr, len })
    }

    /// The prefix's first address.
    pub fn addr(&self) -> IpAddr {
        self.addr
    }

    /// Its length in bits.
    pub fn len(&self) -> u8 {
        self.len
    }

    /// Whether it is the zero-length prefix of its family.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether `ip` is inside the prefix.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, canonical(ip)) {
            (IpAddr::V4(p), IpAddr::V4(a)) => u32::from(a).masked(self.len) == u32::from(p),
            (IpAddr::V6(p), IpAddr::V6(a)) => u128::from(a).masked(self.len) == u128::from(p),
            _ => false,
        }
    }
}

impl FromStr for Prefix {
    type Err = PrefixError;

    /// `192.0.2.0/24`, `2001:db8::/32`, or a bare address as a whole
    /// address prefix (`/32` or `/128`).
    fn from_str(s: &str) -> Result<Self, PrefixError> {
        let (a, len) = match s.split_once('/') {
            Some((a, l)) => {
                let len = l
                    .parse::<u8>()
                    .ok()
                    .filter(|_| !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()))
                    .ok_or_else(|| PrefixError(format!("{s}: {l:?} is not a prefix length")))?;
                (a, Some(len))
            }
            None => (s, None),
        };
        let addr: IpAddr = a
            .parse()
            .map_err(|_| PrefixError(format!("{s}: {a:?} is not an IP address")))?;
        let width = if addr.is_ipv4() { 32 } else { 128 };
        Prefix::new(addr, len.unwrap_or(width))
    }
}

impl fmt::Display for Prefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.len)
    }
}

/// A set of prefixes, each with a value, answering longest-prefix match.
/// See the [module docs](self).
#[derive(Clone, Debug)]
pub struct PrefixList<V> {
    v4: Tree<u32>,
    v6: Tree<u128>,
    values: Vec<V>,
    /// Each stored prefix, in insertion order, by value index.
    prefixes: Vec<Prefix>,
    /// The compiled ranges ([`PrefixList::compile`]), dropped by an insert.
    compiled: Option<(Ranges<u32>, Ranges<u128>)>,
}

impl<V> Default for PrefixList<V> {
    fn default() -> Self {
        Self::new()
    }
}

/// A match: the value of the longest prefix containing the address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match<'a, V> {
    pub prefix: Prefix,
    pub value: &'a V,
}

impl<V> PrefixList<V> {
    pub fn new() -> Self {
        Self {
            v4: Tree::new(),
            v6: Tree::new(),
            values: Vec::new(),
            prefixes: Vec::new(),
            compiled: None,
        }
    }

    /// Builds the range index lookups use once a list is complete, so a
    /// lookup costs a few cache lines instead of a walk down the tree.
    /// Lists from [`PrefixList::parse`] and classifiers are compiled
    /// already; an insert after compiling drops the index again, and
    /// lookups fall back to the tree until the next compile.
    pub fn compile(&mut self) {
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        for (i, p) in self.prefixes.iter().enumerate() {
            match p.addr {
                IpAddr::V4(a) => v4.push((u32::from(a), p.len, i as u32)),
                IpAddr::V6(a) => v6.push((u128::from(a), p.len, i as u32)),
            }
        }
        self.compiled = Some((Ranges::build(v4), Ranges::build(v6)));
    }

    /// Whether lookups use the compiled index.
    pub fn is_compiled(&self) -> bool {
        self.compiled.is_some()
    }

    /// Stores `prefix` with `value`, returning the value it replaced.
    pub fn insert(&mut self, prefix: Prefix, value: V) -> Option<V> {
        self.compiled = None;
        if let Some(i) = self.index_of(&prefix) {
            return Some(std::mem::replace(&mut self.values[i as usize], value));
        }
        let i = self.values.len() as u32;
        self.values.push(value);
        self.prefixes.push(prefix);
        match prefix.addr {
            IpAddr::V4(a) => self.v4.insert(u32::from(a), prefix.len, i),
            IpAddr::V6(a) => self.v6.insert(u128::from(a), prefix.len, i),
        };
        None
    }

    fn index_of(&self, p: &Prefix) -> Option<u32> {
        match p.addr {
            IpAddr::V4(a) => self.v4.exact(u32::from(a), p.len),
            IpAddr::V6(a) => self.v6.exact(u128::from(a), p.len),
        }
    }

    /// The value stored for exactly `prefix`.
    pub fn get(&self, prefix: &Prefix) -> Option<&V> {
        self.index_of(prefix).map(|i| &self.values[i as usize])
    }

    /// The longest stored prefix that contains `ip`, with its value.
    #[inline]
    pub fn lookup(&self, ip: IpAddr) -> Option<Match<'_, V>> {
        fn hit<K: Bits>(r: &Range<K>, a: K) -> Option<(u32, Prefix)> {
            (r.idx != NONE).then(|| {
                (
                    r.idx,
                    Prefix {
                        addr: a.masked(r.len).to_ip(),
                        len: r.len,
                    },
                )
            })
        }
        let found = match (&self.compiled, canonical(ip)) {
            (Some((r4, _)), IpAddr::V4(a)) => hit(r4.lookup(u32::from(a)), u32::from(a)),
            (Some((_, r6)), IpAddr::V6(a)) => hit(r6.lookup(u128::from(a)), u128::from(a)),
            (None, IpAddr::V4(a)) => self
                .v4
                .lookup(u32::from(a))
                .map(|(i, _)| (i, self.prefixes[i as usize])),
            (None, IpAddr::V6(a)) => self
                .v6
                .lookup(u128::from(a))
                .map(|(i, _)| (i, self.prefixes[i as usize])),
        };
        found.map(|(i, prefix)| Match {
            prefix,
            value: &self.values[i as usize],
        })
    }

    /// How many prefixes the list holds.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Every prefix with its value, in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (Prefix, &V)> {
        self.prefixes.iter().copied().zip(&self.values)
    }

    /// Bytes the list's trees, values and prefixes take.
    pub fn memory(&self) -> usize {
        self.v4.nodes.capacity() * std::mem::size_of::<Node<u32>>()
            + self.v6.nodes.capacity() * std::mem::size_of::<Node<u128>>()
            + self.values.capacity() * std::mem::size_of::<V>()
            + self.prefixes.capacity() * std::mem::size_of::<Prefix>()
            + self
                .compiled
                .as_ref()
                .map_or(0, |(a, b)| a.memory() + b.memory())
    }
}

impl PrefixList<()> {
    /// Parses a list file: one prefix per line, `#` starts a comment, and
    /// blank lines are skipped. An error names its line (`line 7:
    /// 10.0.0.1/8: host bits are set past /8`). A prefix listed twice is
    /// kept once.
    pub fn parse(text: &str) -> Result<Self, PrefixError> {
        let mut list = PrefixList::new();
        for (n, line) in text.lines().enumerate() {
            let t = line.split('#').next().unwrap_or("").trim();
            if t.is_empty() {
                continue;
            }
            let p: Prefix = t
                .parse()
                .map_err(|e: PrefixError| PrefixError(format!("line {}: {e}", n + 1)))?;
            list.insert(p, ());
        }
        list.compile();
        Ok(list)
    }

    /// Whether some stored prefix contains `ip`.
    pub fn contains(&self, ip: IpAddr) -> bool {
        self.lookup(ip).is_some()
    }
}

/// A value published to many readers and replaced whole. See the
/// [module docs](self).
#[derive(Debug)]
pub struct Swap<T> {
    generation: AtomicU64,
    current: Mutex<Arc<T>>,
}

impl<T> Swap<T> {
    pub fn new(value: T) -> Self {
        Self {
            generation: AtomicU64::new(0),
            current: Mutex::new(Arc::new(value)),
        }
    }

    /// The value now.
    pub fn load(&self) -> Arc<T> {
        Arc::clone(&self.current.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Publishes `value`. Readers take it at their next lookup; one already
    /// holding the old value keeps it until then.
    pub fn store(&self, value: T) {
        let new = Arc::new(value);
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = new;
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// A reader's handle, holding the value now.
    pub fn reader(self: &Arc<Self>) -> Reader<T> {
        let generation = self.generation.load(Ordering::Acquire);
        Reader {
            swap: Arc::clone(self),
            generation,
            value: self.load(),
        }
    }
}

/// One reader's view of a [`Swap`]: its own reference to the value, kept
/// until the swap's generation moves.
#[derive(Debug)]
pub struct Reader<T> {
    swap: Arc<Swap<T>>,
    generation: u64,
    value: Arc<T>,
}

impl<T> Reader<T> {
    /// The value now: a load of the generation, and the swap's lock only
    /// when a new value was published since the last call.
    #[inline]
    pub fn get(&mut self) -> &T {
        let g = self.swap.generation.load(Ordering::Acquire);
        if g != self.generation {
            self.value = self.swap.load();
            self.generation = g;
        }
        &self.value
    }
}

impl<T> Clone for Reader<T> {
    fn clone(&self) -> Self {
        Self {
            swap: Arc::clone(&self.swap),
            generation: self.generation,
            value: Arc::clone(&self.value),
        }
    }
}

#[cfg(test)]
mod tests;
