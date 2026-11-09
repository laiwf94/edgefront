//! Keyed table: one GCRA state per key (a client address, an API key) with
//! the same rate for every key, ported from libpacer's `table.c`.
//!
//! Memory is allocated once by [`Table::new`] and never grows: the table
//! holds at most `max_entries` keys. It is split into shards, each with its
//! own lock and open-addressing slot array, so threads working on different
//! keys rarely meet. The top bits of a key's SipHash pick its shard and the
//! low bits its home slot; linear probing resolves collisions and deletion
//! shifts later chain members back, so lookups never meet tombstones. Each
//! shard keeps its keys on an intrusive LRU list threaded through the slots.
//!
//! When a new key arrives and its shard is full, the table first reclaims
//! an idle entry (one whose bucket has refilled and that has no holds; it
//! is identical to a fresh entry, so dropping it changes no decision),
//! looking at the eight least recently used. If none is idle, the
//! [`FullPolicy`] decides.
//!
//! libpacer kept each entry's arrival time as absolute ticks and clamped
//! timestamps at 2^57 ns, so past about 4.5 years of uptime its keyed
//! limits stopped refilling. Here each shard has a *base epoch* (see
//! [`crate::clock`]) and its entries keep their arrival times as offsets
//! from the start of it. The base follows the newest timestamp the shard
//! has seen, one epoch behind it, and moves under the shard lock, so any
//! `u64` timestamp works and decisions equal exact arithmetic on absolute
//! time.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::clock::{self, EPOCH_SHIFT, EPOCH_TICKS};
use crate::gcra::{Decision, Error, Rate, TICK_SHIFT, step};
use crate::siphash::{KEY_LEN, KeyHash, random_key};

/// The longest key a table stores, in bytes. An IPv6 address with a port
/// fits with room to spare.
pub const KEY_MAX: usize = 32;
/// The largest `max_entries` a table accepts.
pub const MAX_ENTRIES: u32 = 1 << 28;
/// The largest shard count a table accepts.
pub const MAX_SHARDS: u32 = 65536;

const DEFAULT_SHARDS: u32 = 1024;
const MIN_PER_SHARD: u32 = 64;
const NIL: u32 = u32::MAX;
/// How many entries from the cold end of the LRU list an insert examines
/// looking for an idle one before applying the full-table policy.
const RECLAIM_SCAN: usize = 8;

/// What to do when a new key arrives, its shard is full and none of the
/// eight least recently used entries is idle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FullPolicy {
    /// Refuse the new key with [`Error::TableFull`] and keep everyone's
    /// debt. The safe default against an attacker spraying source
    /// addresses: evicting would let them reset other clients' limits.
    #[default]
    Deny,
    /// Drop the least recently used of those eight that has no holds, and
    /// its debt. Entries with holds are never dropped.
    Evict,
}

/// How to build a [`Table`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableConfig {
    /// Capacity in keys, from 1 to [`MAX_ENTRIES`].
    pub max_entries: u32,
    /// A power of two up to [`MAX_SHARDS`] and at most `max_entries`, or 0
    /// to pick as many as keep at least 64 keys each, up to 1024.
    pub shards: u32,
    /// The rate applied to every key.
    pub rate: Rate,
    /// The SipHash key, or `None` for a random one.
    pub hash_key: Option<[u8; KEY_LEN]>,
    /// What a full shard does with a new key.
    pub full_policy: FullPolicy,
    /// The SipHash variant. [`KeyHash::Sip24`] places keys exactly where
    /// libpacer does.
    pub key_hash: KeyHash,
}

impl TableConfig {
    /// A configuration with automatic sharding, a random hash key, the
    /// deny policy and SipHash-1-3.
    pub const fn new(max_entries: u32, rate: Rate) -> Self {
        Self {
            max_entries,
            shards: 0,
            rate,
            hash_key: None,
            full_policy: FullPolicy::Deny,
            key_hash: KeyHash::Sip13,
        }
    }
}

/// A snapshot of the table's counters, summed over shards.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TableStats {
    /// Keys currently held.
    pub entries: u64,
    /// Maximum keys.
    pub capacity: u64,
    /// Keys admitted since creation.
    pub inserts: u64,
    /// Keys removed explicitly or by a sweep.
    pub removals: u64,
    /// Idle keys dropped to make room.
    pub reclaims: u64,
    /// Keys with debt dropped under [`FullPolicy::Evict`].
    pub evictions: u64,
    /// Keys refused because the table was full.
    pub rejections: u64,
    /// Holds currently open across all keys.
    pub holds: u64,
}

/// One key's state, as reported by [`Table::get`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct EntryState {
    /// The theoretical arrival time in ticks since the clock's start. It
    /// needs more than 64 bits after about 9 years of uptime. An arrival
    /// time before the start of the shard's base epoch, which is idle, reads
    /// as that start (see [`Table`]).
    pub tat: u128,
    /// Holds currently open.
    pub holds: u32,
}

/// One key's state: 64 bytes and aligned to 64, so every entry sits on one
/// cache line and a probe or an LRU update touches one line per entry.
/// libpacer's slot array came from `calloc`, which for a large table
/// returns memory 16 bytes into a line, so three entries in four straddled
/// two lines.
#[derive(Clone, Copy)]
#[repr(align(64))]
struct Entry {
    hash: u64,
    /// The arrival time in ticks from the start of the shard's base epoch.
    tat: u64,
    /// LRU neighbours, as slot indexes in the same shard.
    prev: u32,
    next: u32,
    holds: u32,
    /// 0 marks an empty slot.
    keylen: u8,
    key: [u8; KEY_MAX],
}

const _: () = assert!(std::mem::size_of::<Entry>() == 64 && std::mem::align_of::<Entry>() == 64);

impl Entry {
    const EMPTY: Entry = Entry {
        hash: 0,
        tat: 0,
        prev: 0,
        next: 0,
        holds: 0,
        keylen: 0,
        key: [0; KEY_MAX],
    };

    #[inline]
    fn key(&self) -> &[u8] {
        &self.key[..usize::from(self.keylen)]
    }

    #[inline]
    fn idle(&self, now: u64) -> bool {
        self.tat <= now && self.holds == 0
    }
}

#[derive(Default)]
struct ShardStats {
    inserts: u64,
    removals: u64,
    reclaims: u64,
    evictions: u64,
    rejections: u64,
    holds: u64,
}

struct Shard {
    slots: Box<[Entry]>,
    /// The base epoch: entries' arrival times count from its start. It is
    /// one epoch behind the newest timestamp the shard has seen, or 0.
    base: u64,
    /// Slot count - 1; the count is a power of two.
    mask: u32,
    /// Most keys this shard may hold.
    limit: u32,
    count: u32,
    /// Most recently used.
    head: u32,
    /// Least recently used.
    tail: u32,
    stats: ShardStats,
}

/// A shard behind its lock, alone on its cache lines so that threads
/// locking neighbouring shards do not share a line.
#[repr(align(64))]
struct Padded(Mutex<Shard>);

impl Shard {
    /// `now_ns` in ticks from the start of the base epoch, after moving the
    /// base up when `now_ns` is two or more epochs past it. The result is
    /// below two epochs, 2^63 ticks, so an arrival time up to the deepest
    /// tolerance past it still fits in 64 bits. A timestamp before the
    /// base epoch's start counts as that start.
    #[inline]
    fn now(&mut self, now_ns: u64) -> u64 {
        let epoch = clock::epoch_of(now_ns);
        if epoch > self.base + 1 {
            self.rebase(epoch - 1);
        }
        now_ns.saturating_sub(self.base << EPOCH_SHIFT) << TICK_SHIFT
    }

    /// Move the base epoch up to `base`, shifting every arrival time down
    /// by the epochs it moved. One pass over the slots, at most once every
    /// 2^56 ns (about 2.28 years) per shard. An arrival time that falls
    /// before the new start becomes the start: it is at least an epoch
    /// before any timestamp the shard will accept, so the entry was idle
    /// and stays idle, and no decision changes.
    #[cold]
    #[inline(never)]
    fn rebase(&mut self, base: u64) {
        let shift = u128::from(base - self.base) * u128::from(EPOCH_TICKS);
        for e in self.slots.iter_mut().filter(|e| e.keylen != 0) {
            // Never larger than the old offset, so it fits.
            e.tat = u128::from(e.tat).saturating_sub(shift) as u64;
        }
        self.base = base;
    }

    /// An entry's arrival time in ticks since the clock's start.
    #[inline]
    fn absolute(&self, tat: u64) -> u128 {
        clock::epoch_start(self.base) + u128::from(tat)
    }

    #[inline]
    fn home(&self, hash: u64) -> u32 {
        hash as u32 & self.mask
    }

    #[inline]
    fn at(&mut self, i: u32) -> &mut Entry {
        &mut self.slots[i as usize]
    }

    /// Index of the key's entry, or of the empty slot that ends its probe
    /// chain. The shard is never full of slots (at most three quarters),
    /// so the loop ends.
    fn probe(&self, hash: u64, key: &[u8]) -> u32 {
        let mut i = self.home(hash);
        loop {
            let e = &self.slots[i as usize];
            if e.keylen == 0 || (e.hash == hash && e.key() == key) {
                return i;
            }
            i = (i + 1) & self.mask;
        }
    }

    fn lru_unlink(&mut self, i: u32) {
        let (prev, next) = (self.at(i).prev, self.at(i).next);
        if prev != NIL {
            self.at(prev).next = next;
        } else {
            self.head = next;
        }
        if next != NIL {
            self.at(next).prev = prev;
        } else {
            self.tail = prev;
        }
    }

    fn lru_push_head(&mut self, i: u32) {
        let head = self.head;
        let e = self.at(i);
        e.prev = NIL;
        e.next = head;
        if head != NIL {
            self.at(head).prev = i;
        } else {
            self.tail = i;
        }
        self.head = i;
    }

    #[inline]
    fn lru_touch(&mut self, i: u32) {
        if self.head != i {
            self.lru_unlink(i);
            self.lru_push_head(i);
        }
    }

    /// An entry was copied into slot `to`; point its neighbours at it.
    fn lru_moved(&mut self, to: u32) {
        let (prev, next) = (self.at(to).prev, self.at(to).next);
        if prev != NIL {
            self.at(prev).next = to;
        } else {
            self.head = to;
        }
        if next != NIL {
            self.at(next).prev = to;
        } else {
            self.tail = to;
        }
    }

    /// Delete by shifting later members of the probe chain back into the
    /// hole, so lookups never need tombstones. An entry at `j` may fill
    /// hole `i` only if its home slot is not in the cyclic range `(i, j]`.
    fn delete_at(&mut self, mut i: u32) {
        self.lru_unlink(i);
        let mut j = i;
        loop {
            j = (j + 1) & self.mask;
            let e = self.slots[j as usize];
            if e.keylen == 0 {
                break;
            }
            let home = self.home(e.hash);
            let stays = if i <= j {
                i < home && home <= j
            } else {
                i < home || home <= j
            };
            if stays {
                continue;
            }
            self.slots[i as usize] = e;
            self.lru_moved(i);
            i = j;
        }
        self.slots[i as usize] = Entry::EMPTY;
        self.count -= 1;
    }

    /// Make room for one key in a full shard. Returns whether a slot was
    /// freed. Idle entries near the cold end go first; otherwise the policy
    /// decides.
    fn make_room(&mut self, policy: FullPolicy, now: u64) -> bool {
        if let Some(i) = self.scan_cold(|e| e.idle(now)) {
            self.delete_at(i);
            self.stats.reclaims += 1;
            return true;
        }
        if policy != FullPolicy::Evict {
            return false;
        }
        if let Some(i) = self.scan_cold(|e| e.holds == 0) {
            self.delete_at(i);
            self.stats.evictions += 1;
            return true;
        }
        false
    }

    /// The coldest of the [`RECLAIM_SCAN`] least recently used entries that
    /// satisfies `pick`.
    fn scan_cold(&self, pick: impl Fn(&Entry) -> bool) -> Option<u32> {
        let mut i = self.tail;
        for _ in 0..RECLAIM_SCAN {
            if i == NIL {
                return None;
            }
            let e = &self.slots[i as usize];
            if pick(e) {
                return Some(i);
            }
            i = e.prev;
        }
        None
    }

    /// Find or add the key and mark it most recently used. Returns `None`
    /// when the shard is full and nothing could be reclaimed.
    fn find_or_insert(
        &mut self,
        policy: FullPolicy,
        now: u64,
        hash: u64,
        key: &[u8],
    ) -> Option<&mut Entry> {
        let mut i = self.probe(hash, key);
        if self.slots[i as usize].keylen != 0 {
            self.lru_touch(i);
            return Some(self.at(i));
        }
        if self.count == self.limit {
            if !self.make_room(policy, now) {
                self.stats.rejections += 1;
                return None;
            }
            // Deleting shifted entries around; find the slot again.
            i = self.probe(hash, key);
        }
        let e = self.at(i);
        e.hash = hash;
        e.tat = 0;
        e.holds = 0;
        e.keylen = key.len() as u8;
        e.key[..key.len()].copy_from_slice(key);
        self.lru_push_head(i);
        self.count += 1;
        self.stats.inserts += 1;
        Some(self.at(i))
    }

    /// The key's entry, if present, without touching the LRU order.
    fn find(&mut self, hash: u64, key: &[u8]) -> Option<&mut Entry> {
        let i = self.probe(hash, key);
        let e = self.at(i);
        (e.keylen != 0).then_some(e)
    }

    /// Remove every idle entry in one pass and return how many went.
    fn sweep(&mut self, now_ns: u64) -> usize {
        let now = self.now(now_ns);
        let mut removed = 0;
        let mut i = 0;
        while i <= self.mask {
            let e = &self.slots[i as usize];
            if e.keylen != 0 && e.idle(now) {
                // Another entry may shift into slot i; look at it again.
                self.delete_at(i);
                removed += 1;
                continue;
            }
            i += 1;
        }
        self.stats.removals += removed as u64;
        removed
    }

    fn check(&self) -> Result<(), &'static str> {
        let mut occupied = 0u32;
        for (i, e) in self.slots.iter().enumerate() {
            if e.keylen == 0 {
                continue;
            }
            occupied += 1;
            if self.probe(e.hash, e.key()) as usize != i {
                return Err("a key is not found by a probe from its home slot");
            }
        }
        let (mut linked, mut prev, mut i) = (0u32, NIL, self.head);
        while i != NIL {
            let e = &self.slots[i as usize];
            linked += 1;
            if e.keylen == 0 || e.prev != prev || linked > occupied {
                return Err("the LRU list is not linked both ways over occupied slots");
            }
            prev = i;
            i = e.next;
        }
        if prev != self.tail || linked != occupied || occupied != self.count {
            return Err("the LRU list, the slots and the count disagree");
        }
        Ok(())
    }
}

/// A fixed-memory, sharded table of per-key GCRA limiters. Every method is
/// thread-safe; keys are copied.
///
/// Arrival times count from each shard's base epoch, which a timestamp two
/// or more epochs past it moves up to the epoch before that timestamp's,
/// under the shard lock, in one pass over the shard's slots. Any `u64`
/// timestamp works, and decisions are exact arithmetic on absolute time.
/// As with [`crate::Gcra`], callers sharing a table must use timestamps
/// within one epoch (2^56 ns, about 2.28 years) of each other, which any
/// monotonic clock gives: a timestamp before the start of a shard's base
/// epoch counts as that start.
pub struct Table {
    shards: Box<[Padded]>,
    /// The top bits of a hash pick the shard; 64 for a single shard.
    shard_shift: u32,
    capacity: u32,
    policy: FullPolicy,
    sweep_cursor: AtomicU32,
    memory: usize,
    interval: AtomicU64,
    tolerance: AtomicU64,
    hash_key: [u8; KEY_LEN],
    key_hash: KeyHash,
}

impl std::fmt::Debug for Table {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Table")
            .field("shards", &self.shards.len())
            .field("capacity", &self.capacity)
            .field("policy", &self.policy)
            .field("key_hash", &self.key_hash)
            .finish_non_exhaustive()
    }
}

/// As many shards as keep at least [`MIN_PER_SHARD`] keys each, up to 1024.
/// More shards mean fewer threads meeting on one lock; with too few keys
/// per shard, hashing imbalance fills some shards while others still have
/// room.
fn pick_shards(max_entries: u32) -> u32 {
    let mut n = DEFAULT_SHARDS;
    while n > 1 && max_entries / n < MIN_PER_SHARD {
        n >>= 1;
    }
    n
}

#[inline]
fn check_key(key: &[u8]) -> Result<(), Error> {
    if key.is_empty() || key.len() > KEY_MAX {
        Err(Error::InvalidKey)
    } else {
        Ok(())
    }
}

impl Table {
    /// Allocate a table. All of its memory is allocated here.
    pub fn new(cfg: &TableConfig) -> Result<Self, Error> {
        if cfg.max_entries == 0 || cfg.max_entries > MAX_ENTRIES {
            return Err(Error::InvalidConfig);
        }
        let nshards = if cfg.shards == 0 {
            pick_shards(cfg.max_entries)
        } else {
            cfg.shards
        };
        if nshards > MAX_SHARDS || !nshards.is_power_of_two() || nshards > cfg.max_entries {
            return Err(Error::InvalidConfig);
        }
        let (iv, tol) = cfg.rate.ticks()?;

        // Spread max_entries over the shards exactly, then size each slot
        // array so it is at most three quarters full.
        let base = cfg.max_entries / nshards;
        let extra = cfg.max_entries % nshards;
        let slots_per_shard = ((base + 1) * 4).div_ceil(3).next_power_of_two();
        let shards: Box<[Padded]> = (0..nshards)
            .map(|i| {
                Padded(Mutex::new(Shard {
                    slots: vec![Entry::EMPTY; slots_per_shard as usize].into_boxed_slice(),
                    mask: slots_per_shard - 1,
                    limit: base + u32::from(i < extra),
                    base: 0,
                    count: 0,
                    head: NIL,
                    tail: NIL,
                    stats: ShardStats::default(),
                }))
            })
            .collect();
        let memory = std::mem::size_of::<Self>()
            + std::mem::size_of::<Padded>() * nshards as usize
            + std::mem::size_of::<Entry>() * slots_per_shard as usize * nshards as usize;
        Ok(Self {
            shards,
            shard_shift: 64 - nshards.trailing_zeros(),
            capacity: cfg.max_entries,
            policy: cfg.full_policy,
            sweep_cursor: AtomicU32::new(0),
            memory,
            interval: AtomicU64::new(iv),
            tolerance: AtomicU64::new(tol),
            hash_key: cfg.hash_key.unwrap_or_else(random_key),
            key_hash: cfg.key_hash,
        })
    }

    #[inline]
    fn rate(&self) -> (u64, u64) {
        (
            self.interval.load(Ordering::Relaxed),
            self.tolerance.load(Ordering::Relaxed),
        )
    }

    /// The key's hash and its shard, locked. A poisoned lock is taken
    /// anyway: no shard operation can panic halfway through an update.
    #[inline]
    fn lock(&self, key: &[u8]) -> (u64, MutexGuard<'_, Shard>) {
        let hash = self.key_hash.hash(&self.hash_key, key);
        // A single shard has a shift of 64, which checked_shr maps to 0.
        let idx = hash.checked_shr(self.shard_shift).unwrap_or(0) as usize;
        (hash, lock(&self.shards[idx]))
    }

    /// Take `cost` tokens for `key` at `now_ns`, all or nothing, adding the
    /// key if needed. Fails with [`Error::InvalidKey`],
    /// [`Error::CostExceedsBurst`] (before the key is added), or
    /// [`Error::TableFull`] when the key is new and there is no room.
    pub fn take(&self, key: &[u8], now_ns: u64, cost: u64) -> Result<Decision, Error> {
        check_key(key)?;
        let (iv, tol) = self.rate();
        if cost > tol / iv {
            return Err(Error::CostExceedsBurst);
        }
        let (hash, mut s) = self.lock(key);
        let now = s.now(now_ns);
        let e = s
            .find_or_insert(self.policy, now, hash, key)
            .ok_or(Error::TableFull)?;
        let mut d = Decision::default();
        if let Some(next) = step::take(e.tat, now, cost, iv, tol, &mut d) {
            e.tat = next;
        }
        Ok(d)
    }

    /// Take as many tokens as are available for `key`, up to `max`. When
    /// nothing is granted, `retry_after_ns` says when one token will be.
    pub fn take_upto(&self, key: &[u8], now_ns: u64, max: u64) -> Result<Decision, Error> {
        self.take_range(key, now_ns, 1, max)
    }

    /// [`crate::Gcra::take_range`] for one key: grant nothing unless at
    /// least `min` tokens are available, otherwise as many as are, up to
    /// `max`.
    pub fn take_range(
        &self,
        key: &[u8],
        now_ns: u64,
        min: u64,
        max: u64,
    ) -> Result<Decision, Error> {
        check_key(key)?;
        let (iv, tol) = self.rate();
        let (hash, mut s) = self.lock(key);
        let now = s.now(now_ns);
        let e = s
            .find_or_insert(self.policy, now, hash, key)
            .ok_or(Error::TableFull)?;
        let mut d = Decision::default();
        if let Some((_, next)) = step::range(e.tat, now, min, max, iv, tol, &mut d) {
            e.tat = next;
        }
        Ok(d)
    }

    /// Hand back unused tokens for `key`. Unknown and invalid keys are
    /// ignored: an evicted key is already at a full bucket.
    pub fn refund(&self, key: &[u8], now_ns: u64, tokens: u64) {
        if check_key(key).is_err() {
            return;
        }
        let iv = self.interval.load(Ordering::Relaxed);
        let (hash, mut s) = self.lock(key);
        let now = s.now(now_ns);
        if let Some(e) = s.find(hash, key) {
            e.tat = step::refund(e.tat, now, tokens, iv);
        }
    }

    /// Concurrency holds: count how many things (connections) a key has
    /// open. Admits one more if the key has fewer than `max_holds` and a
    /// take of one token is granted, so one call enforces both a
    /// concurrency cap and an arrival rate. A rate denial is an `Ok`
    /// decision with `allowed` false; at the cap it fails with
    /// [`Error::HoldLimit`] and takes no token. Every admitted hold must be
    /// matched by exactly one [`Table::release`].
    pub fn hold(&self, key: &[u8], now_ns: u64, max_holds: u32) -> Result<Decision, Error> {
        check_key(key)?;
        let (iv, tol) = self.rate();
        let (hash, mut s) = self.lock(key);
        let now = s.now(now_ns);
        let e = s
            .find_or_insert(self.policy, now, hash, key)
            .ok_or(Error::TableFull)?;
        if e.holds >= max_holds {
            return Err(Error::HoldLimit);
        }
        let mut d = Decision::default();
        if let Some(next) = step::take(e.tat, now, 1, iv, tol, &mut d) {
            e.tat = next;
            e.holds += 1;
            s.stats.holds += 1;
        }
        Ok(d)
    }

    /// Drop one hold. Fails with [`Error::NoSuchKey`] if the key has none,
    /// which means the caller released more than it held.
    pub fn release(&self, key: &[u8]) -> Result<(), Error> {
        check_key(key)?;
        let (hash, mut s) = self.lock(key);
        match s.find(hash, key) {
            Some(e) if e.holds > 0 => {
                e.holds -= 1;
                s.stats.holds -= 1;
                Ok(())
            }
            _ => Err(Error::NoSuchKey),
        }
    }

    /// Remove `key`, holds and all.
    pub fn remove(&self, key: &[u8]) -> Result<(), Error> {
        check_key(key)?;
        let (hash, mut s) = self.lock(key);
        let i = s.probe(hash, key);
        let holds = s.slots[i as usize].holds;
        if s.slots[i as usize].keylen == 0 {
            return Err(Error::NoSuchKey);
        }
        s.stats.holds -= u64::from(holds);
        s.delete_at(i);
        s.stats.removals += 1;
        Ok(())
    }

    /// The key's state, without adding it or changing its recency.
    pub fn get(&self, key: &[u8]) -> Option<EntryState> {
        check_key(key).ok()?;
        let (hash, mut s) = self.lock(key);
        let (tat, holds) = s.find(hash, key).map(|e| (e.tat, e.holds))?;
        Some(EntryState {
            tat: s.absolute(tat),
            holds,
        })
    }

    /// Change the per-key rate for every key, keeping debt as time. An
    /// invalid rate leaves the table unchanged.
    pub fn reconfigure(&self, rate: &Rate) -> Result<(), Error> {
        let (iv, tol) = rate.ticks()?;
        self.interval.store(iv, Ordering::Relaxed);
        self.tolerance.store(tol, Ordering::Relaxed);
        Ok(())
    }

    /// Remove idle entries from the next `nshards` shards (round robin
    /// across calls) and return how many were removed. Each shard is
    /// scanned under its lock in one pass. Calling it is optional: inserts
    /// reclaim idle entries on their own; sweeping keeps `entries` honest
    /// and room ready.
    pub fn sweep(&self, now_ns: u64, nshards: u32) -> usize {
        let n = self.shards.len();
        let mut removed = 0;
        for _ in 0..(nshards as usize).min(n) {
            let idx = self.sweep_cursor.fetch_add(1, Ordering::Relaxed) as usize;
            removed += lock(&self.shards[idx & (n - 1)]).sweep(now_ns);
        }
        removed
    }

    /// A snapshot of the counters, summed over shards.
    pub fn stats(&self) -> TableStats {
        let mut out = TableStats {
            capacity: u64::from(self.capacity),
            ..TableStats::default()
        };
        for p in &self.shards {
            let s = lock(p);
            out.entries += u64::from(s.count);
            out.inserts += s.stats.inserts;
            out.removals += s.stats.removals;
            out.reclaims += s.stats.reclaims;
            out.evictions += s.stats.evictions;
            out.rejections += s.stats.rejections;
            out.holds += s.stats.holds;
        }
        out
    }

    /// Bytes allocated by the table, fixed at creation.
    pub fn memory(&self) -> usize {
        self.memory
    }

    /// The number of shards.
    pub fn shards(&self) -> u32 {
        self.shards.len() as u32
    }

    /// Walk every shard under its lock and verify that the LRU list links
    /// each occupied slot exactly once in both directions, that the count
    /// matches, and that every key is found by a probe from its home slot.
    /// For tests; the cost is linear in the table size.
    pub fn check_invariants(&self) -> Result<(), &'static str> {
        self.shards.iter().try_for_each(|p| lock(p).check())
    }
}

#[inline]
fn lock(p: &Padded) -> MutexGuard<'_, Shard> {
    p.0.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    //! Ported one for one from libpacer's tests/test_table.c, plus a
    //! reference-model comparison and threaded checks.

    use super::*;
    use crate::clock::EPOCH_NS;
    use crate::gcra::wide;
    use crate::siphash;
    use std::collections::{HashMap, VecDeque};
    use std::sync::Arc;

    const SEC: u64 = 1_000_000_000;
    const FIXED_KEY: [u8; KEY_LEN] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

    fn config(max_entries: u32, shards: u32, rate: Rate, policy: FullPolicy) -> TableConfig {
        TableConfig {
            max_entries,
            shards,
            rate,
            hash_key: Some(FIXED_KEY),
            full_policy: policy,
            key_hash: KeyHash::Sip24,
        }
    }

    fn make_table(max_entries: u32, shards: u32, burst: u64) -> Table {
        Table::new(&config(
            max_entries,
            shards,
            Rate::new(10, SEC, burst),
            FullPolicy::Deny,
        ))
        .unwrap()
    }

    fn make_policy_table(max_entries: u32, policy: FullPolicy) -> Table {
        Table::new(&config(max_entries, 1, Rate::new(10, SEC, 1), policy)).unwrap()
    }

    fn key_name(i: u32) -> String {
        format!("10.0.{}.{}", i / 256, i % 256)
    }

    fn take_key(t: &Table, i: u32, now: u64) -> Result<bool, Error> {
        t.take(key_name(i).as_bytes(), now, 1).map(|d| d.allowed)
    }

    /// Each key takes its whole burst, so a later take tells whether the
    /// entry is still there (denied) or was lost (admitted fresh).
    fn key_present(t: &Table, i: u32) -> bool {
        take_key(t, i, SEC) == Ok(false)
    }

    #[test]
    fn rejects_bad_config() {
        let rate = Rate::new(10, SEC, 1);
        let bad = |max, shards, rate| {
            Table::new(&config(max, shards, rate, FullPolicy::Deny)).unwrap_err()
        };
        assert_eq!(bad(0, 0, rate), Error::InvalidConfig);
        assert_eq!(bad(100, 3, rate), Error::InvalidConfig);
        assert_eq!(bad(100, 128, rate), Error::InvalidConfig);
        assert_eq!(bad(1 << 20, 1 << 17, rate), Error::InvalidConfig);
        assert_eq!(bad(100, 0, Rate::new(0, SEC, 1)), Error::InvalidRate);
        assert_eq!(bad(MAX_ENTRIES + 1, 0, rate), Error::InvalidConfig);
        // libpacer also rejected an unknown policy number and NULL
        // pointers; FullPolicy and references make those unrepresentable.
    }

    #[test]
    fn errors_describe_themselves() {
        assert_eq!(
            Error::InvalidConfig.to_string(),
            "invalid table configuration"
        );
        assert_eq!(Error::InvalidKey.to_string(), "invalid key");
        assert_eq!(Error::TableFull.to_string(), "table full");
        assert_eq!(Error::NoSuchKey.to_string(), "no such key");
        assert_eq!(Error::HoldLimit.to_string(), "concurrency limit reached");
    }

    #[test]
    fn picks_shards_with_at_least_64_keys_each() {
        for (max, want) in [(1, 1), (127, 1), (128, 2), (65536, 1024), (1 << 28, 1024)] {
            assert_eq!(pick_shards(max), want, "{max}");
        }
        assert_eq!(make_table(1000, 0, 1).shards(), 8);
        assert_eq!(make_table(1000, 16, 1).shards(), 16);
    }

    #[test]
    fn keys_are_independent() {
        let t = make_table(1000, 0, 2);
        assert!(t.take(b"a", SEC, 2).unwrap().allowed);
        assert!(!t.take(b"a", SEC, 1).unwrap().allowed);
        assert!(t.take(b"b", SEC, 1).unwrap().allowed);
        assert!(t.take(b"ab", SEC, 2).unwrap().allowed);
        let d = t.take(b"a", SEC, 1).unwrap();
        assert!(!d.allowed);
        assert_eq!(d.retry_after_ns, SEC / 10);
        assert!(t.take(b"a", SEC + SEC / 10, 1).unwrap().allowed);
        let st = t.stats();
        assert_eq!(st.entries, 3);
        assert_eq!(st.inserts, 3);
        assert_eq!(st.capacity, 1000);
    }

    #[test]
    fn rejects_bad_keys_and_costs() {
        let t = make_table(100, 0, 2);
        let long_key = [b'k'; KEY_MAX + 1];
        assert_eq!(t.take(b"", SEC, 1), Err(Error::InvalidKey));
        assert_eq!(t.take(&long_key, SEC, 1), Err(Error::InvalidKey));
        assert!(t.take(&long_key[..KEY_MAX], SEC, 1).unwrap().allowed);
        assert_eq!(t.take(b"a", SEC, 3), Err(Error::CostExceedsBurst));
        assert_eq!(t.get(b"a"), None, "a rejected cost adds no key");
        assert_eq!(t.take_upto(b"", SEC, 1), Err(Error::InvalidKey));
        assert_eq!(t.remove(b""), Err(Error::InvalidKey));
        assert_eq!(t.get(b""), None);
        assert_eq!(t.get(&long_key), None);
        t.refund(b"", SEC, 1);
        t.refund(&long_key, SEC, 1);
        assert_eq!(t.stats().entries, 1);
        // libpacer also rejected a NULL key and a NULL decision for
        // take_upto; slices and returned decisions make those impossible.
    }

    #[test]
    fn full_refuses_new_keys() {
        let t = make_table(12, 1, 1);
        for i in 0..12 {
            assert_eq!(take_key(&t, i, SEC), Ok(true));
        }
        let k = key_name(99);
        assert_eq!(t.take(k.as_bytes(), SEC, 1), Err(Error::TableFull));
        assert_eq!(t.take_upto(k.as_bytes(), SEC, 1), Err(Error::TableFull));
        assert_eq!(t.hold(k.as_bytes(), SEC, 1), Err(Error::TableFull));
        let st = t.stats();
        assert_eq!(st.entries, 12);
        assert_eq!(st.rejections, 3);
        // Existing keys still work when the table is full.
        assert_eq!(take_key(&t, 3, 2 * SEC), Ok(true));
    }

    #[test]
    fn delete_keeps_probe_chains_intact() {
        // 48 keys in 64 slots of one shard produce long probe chains.
        // Remove keys in a scrambled order and check every survivor after
        // each removal.
        let n = 48;
        let t = make_table(n, 1, 1);
        for i in 0..n {
            assert_eq!(take_key(&t, i, SEC), Ok(true));
        }
        let mut removed = vec![false; n as usize];
        for step in 0..n {
            let victim = (step * 29 + 7) % n;
            let k = key_name(victim);
            assert_eq!(t.remove(k.as_bytes()), Ok(()));
            assert_eq!(t.remove(k.as_bytes()), Err(Error::NoSuchKey));
            t.check_invariants().unwrap();
            removed[victim as usize] = true;
            let lost = (0..n)
                .filter(|&i| !removed[i as usize] && !key_present(&t, i))
                .count();
            assert_eq!(lost, 0, "after removing {victim}");
        }
        let st = t.stats();
        assert_eq!(st.entries, 0);
        assert_eq!(st.removals, u64::from(n));
    }

    #[test]
    fn delete_wraps_around_the_end_of_the_slot_array() {
        // Find keys whose home is the last slot of a one-shard table, so
        // their probe chain wraps to slot 0, then delete from the middle.
        let t = make_table(48, 1, 1);
        let mask = 63;
        let wrapping: Vec<String> = (0..)
            .map(key_name)
            .filter(|k| siphash::siphash24(&FIXED_KEY, k.as_bytes()) as u32 & mask == mask)
            .take(4)
            .collect();
        for k in &wrapping {
            assert!(t.take(k.as_bytes(), SEC, 1).unwrap().allowed);
        }
        t.check_invariants().unwrap();
        assert_eq!(t.remove(wrapping[0].as_bytes()), Ok(()));
        t.check_invariants().unwrap();
        for k in &wrapping[1..] {
            assert!(!t.take(k.as_bytes(), SEC, 1).unwrap().allowed, "{k} lost");
        }
    }

    #[test]
    fn refund_after_a_shrink_gives_back_every_token() {
        // 10 tokens of debt at 10/s is 1 s; then the burst shrinks to 1.
        // Refunding 6 tokens leaves 400 ms of debt, so the next token is
        // 400 ms away. A refund capped at one burst would leave it 900 ms
        // away.
        let t = make_table(100, 0, 100);
        assert_eq!(t.take_upto(b"k", SEC, 10).unwrap().granted, 10);
        t.reconfigure(&Rate::new(10, SEC, 1)).unwrap();
        t.refund(b"k", SEC, 6);
        let d = t.take(b"k", SEC, 1).unwrap();
        assert!(!d.allowed);
        assert_eq!(d.retry_after_ns, 4 * SEC / 10);
    }

    #[test]
    fn take_upto_and_refund() {
        let t = make_table(100, 0, 100);
        assert_eq!(t.take_upto(b"k", SEC, 70).unwrap().granted, 70);
        assert_eq!(t.take_upto(b"k", SEC, 70).unwrap().granted, 30);
        let d = t.take_upto(b"k", SEC, 70).unwrap();
        assert_eq!(d.granted, 0);
        assert_eq!(d.retry_after_ns, SEC / 10);
        t.refund(b"k", SEC, 20);
        assert_eq!(t.take_upto(b"k", SEC, 70).unwrap().granted, 20);
        // Refunding an unknown key is harmless.
        t.refund(b"zz", SEC, 20);
        assert_eq!(t.stats().entries, 1);
    }

    #[test]
    fn reconfigure_applies_to_every_key() {
        let t = make_table(100, 0, 1);
        assert!(t.take(b"a", SEC, 1).unwrap().allowed);
        t.reconfigure(&Rate::new(10, SEC, 5)).unwrap();
        assert!(t.take(b"b", SEC, 5).unwrap().allowed);
        assert!(t.take(b"a", SEC, 4).unwrap().allowed);
        assert_eq!(
            t.reconfigure(&Rate::new(0, SEC, 5)),
            Err(Error::InvalidRate)
        );
        assert!(t.take(b"c", SEC, 5).unwrap().allowed, "rate unchanged");
    }

    #[test]
    fn memory_is_fixed_at_create() {
        let t = make_table(65536, 0, 1);
        let before = t.memory();
        // 65536 keys in 1024 shards of 64, each in 128 slots of 64 bytes:
        // 8 MiB of entries plus the shard headers.
        assert!(before >= 1024 * 128 * 64);
        assert!(before < 1024 * 128 * 64 + 1024 * 256);
        for i in 0..70000 {
            let _ = take_key(&t, i, SEC);
        }
        assert_eq!(t.memory(), before);
        let st = t.stats();
        assert!(st.entries <= 65536);
        assert_eq!(st.entries + st.rejections, 70000);
    }

    #[test]
    fn random_hash_key() {
        let mut cfg = config(10, 0, Rate::new(10, SEC, 1), FullPolicy::Deny);
        cfg.hash_key = None;
        let t = Table::new(&cfg).unwrap();
        assert!(t.take(b"a", SEC, 1).unwrap().allowed);
        assert!(!t.take(b"a", SEC, 1).unwrap().allowed);
        let t = Table::new(&TableConfig::new(10, Rate::new(10, SEC, 1))).unwrap();
        assert!(t.take(b"a", SEC, 1).unwrap().allowed);
        assert!(format!("{t:?}").contains("Sip13"));
    }

    #[test]
    fn reclaims_idle_entries_without_a_sweep() {
        // At 10/s with burst 1, an entry is idle 100 ms after its last take.
        let t = make_policy_table(16, FullPolicy::Deny);
        for i in 0..16 {
            assert_eq!(take_key(&t, i, SEC), Ok(true));
        }
        assert_eq!(take_key(&t, 100, SEC), Err(Error::TableFull));
        let later = SEC + SEC / 10;
        for i in 100..116 {
            assert_eq!(take_key(&t, i, later), Ok(true));
        }
        let st = t.stats();
        assert_eq!(st.entries, 16);
        assert_eq!(st.reclaims, 16);
        assert_eq!(st.rejections, 1);
    }

    #[test]
    fn deny_policy_keeps_active_debt() {
        let t = make_policy_table(16, FullPolicy::Deny);
        for i in 0..16 {
            assert_eq!(take_key(&t, i, SEC), Ok(true));
        }
        for i in 100..200 {
            assert_eq!(take_key(&t, i, SEC), Err(Error::TableFull));
        }
        // Every original key still owes its debt.
        for i in 0..16 {
            assert_eq!(take_key(&t, i, SEC), Ok(false));
        }
    }

    #[test]
    fn evict_policy_drops_least_recently_used() {
        let t = make_policy_table(16, FullPolicy::Evict);
        for i in 0..16 {
            assert_eq!(take_key(&t, i, SEC), Ok(true));
        }
        // Touch key 0 so key 1 becomes the coldest.
        assert_eq!(take_key(&t, 0, SEC), Ok(false));
        assert_eq!(take_key(&t, 100, SEC), Ok(true));
        assert_eq!(take_key(&t, 1, SEC), Ok(true), "forgotten, so fresh");
        assert_eq!(take_key(&t, 0, SEC), Ok(false), "still remembered");
        let st = t.stats();
        assert_eq!(st.evictions, 2);
        assert_eq!(st.entries, 16);
    }

    #[test]
    fn evict_gives_up_when_the_eight_coldest_are_held() {
        let t = make_policy_table(16, FullPolicy::Evict);
        for i in 0..16 {
            assert!(t.hold(key_name(i).as_bytes(), SEC, 1).unwrap().allowed);
        }
        // Make the eight coldest the ones still held: release the warmest
        // eight, then touch them so they move to the warm end.
        for i in 8..16 {
            t.release(key_name(i).as_bytes()).unwrap();
            let _ = take_key(&t, i, SEC);
        }
        assert_eq!(take_key(&t, 100, SEC), Err(Error::TableFull));
        assert_eq!(t.stats().rejections, 1);
    }

    #[test]
    fn sweep_removes_only_idle_entries() {
        let t = Table::new(&config(1024, 4, Rate::new(10, SEC, 1), FullPolicy::Deny)).unwrap();
        for i in 0..500 {
            assert_eq!(take_key(&t, i, SEC), Ok(true));
        }
        for i in 500..800 {
            assert_eq!(take_key(&t, i, 2 * SEC), Ok(true));
        }
        // At 2.05 s the first 500 are idle and the last 300 are not.
        assert_eq!(t.sweep(2 * SEC + SEC / 20, 100), 500);
        t.check_invariants().unwrap();
        assert_eq!(t.stats().entries, 300);
        for i in 500..800 {
            assert_eq!(take_key(&t, i, 2 * SEC), Ok(false));
        }
        // Sweeping one shard at a time visits them round robin.
        let removed: usize = (0..4).map(|_| t.sweep(10 * SEC, 1)).sum();
        assert_eq!(removed, 300);
        assert_eq!(t.stats().removals, 800);
    }

    #[test]
    fn lru_survives_heavy_churn() {
        // Random-looking churn with removes, reclaims and evictions; the
        // LRU list must stay consistent with the slot array throughout.
        let t = make_policy_table(64, FullPolicy::Evict);
        let mut now = SEC;
        for step in 0..20000u32 {
            let k = step.wrapping_mul(2654435761) % 300;
            if step % 7 == 0 {
                let _ = t.remove(key_name(k).as_bytes());
            } else {
                assert!(take_key(&t, k, now).is_ok());
            }
            now += SEC / 1000;
            t.check_invariants().unwrap();
        }
        let st = t.stats();
        assert!(st.entries <= 64);
        assert_eq!(
            st.inserts,
            st.entries + st.removals + st.reclaims + st.evictions
        );
    }

    #[test]
    fn hold_caps_concurrency() {
        let t = Table::new(&config(100, 1, Rate::new(1000, SEC, 100), FullPolicy::Deny)).unwrap();
        for _ in 0..3 {
            assert!(t.hold(b"ip", SEC, 3).unwrap().allowed);
        }
        assert_eq!(t.hold(b"ip", SEC, 3), Err(Error::HoldLimit));
        assert!(t.hold(b"other", SEC, 3).unwrap().allowed);
        assert_eq!(t.release(b"ip"), Ok(()));
        assert!(t.hold(b"ip", SEC, 3).unwrap().allowed);
        assert_eq!(t.stats().holds, 4);
        assert_eq!(t.get(b"ip").unwrap().holds, 3);
        // The cap check costs no token: 4 grants out of a burst of 100.
        assert!(t.take(b"ip", SEC, 96).unwrap().allowed);
    }

    #[test]
    fn hold_respects_the_rate() {
        let t = Table::new(&config(100, 1, Rate::new(10, SEC, 2), FullPolicy::Deny)).unwrap();
        assert!(t.hold(b"ip", SEC, 100).unwrap().allowed);
        assert!(t.hold(b"ip", SEC, 100).unwrap().allowed);
        let d = t.hold(b"ip", SEC, 100).unwrap();
        assert!(!d.allowed);
        assert_eq!(d.retry_after_ns, SEC / 10);
        assert!(t.hold(b"ip", SEC + SEC / 10, 100).unwrap().allowed);
        assert_eq!(t.hold(b"", SEC, 1), Err(Error::InvalidKey));
        assert_eq!(t.get(b"ip").unwrap().holds, 3);
    }

    #[test]
    fn release_without_hold_is_an_error() {
        let t = make_policy_table(16, FullPolicy::Deny);
        assert_eq!(t.release(b"nobody"), Err(Error::NoSuchKey));
        assert_eq!(take_key(&t, 1, SEC), Ok(true));
        assert_eq!(t.release(key_name(1).as_bytes()), Err(Error::NoSuchKey));
        assert_eq!(t.release(b""), Err(Error::InvalidKey));
    }

    #[test]
    fn held_entries_are_never_dropped() {
        let t = make_policy_table(16, FullPolicy::Evict);
        for i in 0..16 {
            assert!(t.hold(key_name(i).as_bytes(), SEC, 10).unwrap().allowed);
        }
        // Long after their buckets refilled, held entries are neither idle
        // nor evictable, so a new key is refused even under Evict.
        assert_eq!(take_key(&t, 100, 100 * SEC), Err(Error::TableFull));
        assert_eq!(t.sweep(100 * SEC, 1), 0);
        // Releasing one makes it idle and reclaimable.
        assert_eq!(t.release(key_name(5).as_bytes()), Ok(()));
        assert_eq!(take_key(&t, 100, 100 * SEC), Ok(true));
        let st = t.stats();
        assert_eq!(st.reclaims, 1);
        assert_eq!(st.holds, 15);
        // Removing a held key drops its holds from the count.
        assert_eq!(t.remove(key_name(6).as_bytes()), Ok(()));
        assert_eq!(t.stats().holds, 14);
        t.check_invariants().unwrap();
    }

    #[test]
    fn evict_skips_held_entries() {
        let t = make_policy_table(16, FullPolicy::Evict);
        let k0 = key_name(0);
        assert!(t.hold(k0.as_bytes(), SEC, 10).unwrap().allowed);
        for i in 1..16 {
            assert_eq!(take_key(&t, i, SEC), Ok(true));
        }
        // Key 0 is the coldest but held, so key 1 is evicted instead.
        assert_eq!(take_key(&t, 100, SEC), Ok(true));
        assert_eq!(take_key(&t, 1, SEC), Ok(true));
        assert_eq!(t.release(k0.as_bytes()), Ok(()));
    }

    #[test]
    fn take_range() {
        let t = make_table(100, 0, 100);
        assert_eq!(t.take_range(b"k", SEC, 1, 95).unwrap().granted, 95);
        let d = t.take_range(b"k", SEC, 10, 95).unwrap();
        assert_eq!(d.granted, 0);
        assert_eq!(d.retry_after_ns, SEC / 2);
    }

    #[test]
    fn get_reports_state_without_touching_recency() {
        let t = make_policy_table(16, FullPolicy::Evict);
        assert_eq!(t.get(b"k"), None);
        for i in 0..16 {
            assert_eq!(take_key(&t, i, SEC), Ok(true));
        }
        // Key 0 is the coldest; reading it must not save it.
        let st = t.get(key_name(0).as_bytes()).unwrap();
        assert_eq!(
            st.tat,
            clock::ticks_wide(SEC) + u128::from(t.interval.load(Ordering::Relaxed))
        );
        assert_eq!(st.holds, 0);
        assert_eq!(take_key(&t, 100, SEC), Ok(true));
        assert_eq!(t.get(key_name(0).as_bytes()), None);
        assert_eq!(t.stats().entries, 16, "get adds nothing");
    }

    #[test]
    fn sip13_tables_decide_like_sip24_tables() {
        // The hash variant moves keys between shards and slots but, with
        // room for every key, never changes a decision.
        let mut cfg = config(4096, 0, Rate::new(10, SEC, 3), FullPolicy::Deny);
        let a = Table::new(&cfg).unwrap();
        cfg.key_hash = KeyHash::Sip13;
        let b = Table::new(&cfg).unwrap();
        for step in 0..20000u32 {
            let k = key_name(step.wrapping_mul(2654435761) % 2000);
            let now = SEC + u64::from(step) * (SEC / 1000);
            assert_eq!(a.take(k.as_bytes(), now, 1), b.take(k.as_bytes(), now, 1));
        }
        assert_eq!(a.stats(), b.stats());
    }

    #[test]
    fn a_poisoned_shard_keeps_working() {
        let t = Arc::new(make_table(100, 1, 2));
        let t2 = Arc::clone(&t);
        let _ = std::thread::spawn(move || {
            let _guard = t2.shards[0].0.lock().unwrap();
            panic!("poison the shard");
        })
        .join();
        assert!(t.shards[0].0.is_poisoned());
        assert!(t.take(b"a", SEC, 1).unwrap().allowed);
        t.check_invariants().unwrap();
    }

    /// splitmix64, for reproducible random scripts.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A one-shard table written the obvious way: a map of states and a
    /// recency queue, coldest first, with arrival times as exact absolute
    /// ticks in 128 bits. It knows nothing of slots, probing, intrusive
    /// links or epochs, so agreeing with it checks those.
    struct Model {
        limit: usize,
        policy: FullPolicy,
        iv: u128,
        tol: u128,
        states: HashMap<Vec<u8>, EntryState>,
        lru: VecDeque<Vec<u8>>,
        /// The newest epoch the shard has been given a timestamp in.
        newest_epoch: u64,
    }

    impl Model {
        fn new(limit: u32, policy: FullPolicy, rate: Rate) -> Self {
            let (iv, tol) = rate.ticks().unwrap();
            Self {
                limit: limit as usize,
                policy,
                iv: u128::from(iv),
                tol: u128::from(tol),
                states: HashMap::new(),
                lru: VecDeque::new(),
                newest_epoch: 0,
            }
        }

        /// `now_ns` in absolute ticks, noting its epoch.
        fn at(&mut self, now_ns: u64) -> u128 {
            self.newest_epoch = self.newest_epoch.max(clock::epoch_of(now_ns));
            clock::ticks_wide(now_ns)
        }

        /// What [`Table::get`] reports: the exact state, except that an
        /// arrival time before the shard's base epoch, one behind the
        /// newest, reads as its start.
        fn get(&self, key: &[u8]) -> Option<EntryState> {
            let floor = clock::epoch_start(self.newest_epoch.saturating_sub(1));
            self.states.get(key).map(|e| EntryState {
                tat: e.tat.max(floor),
                holds: e.holds,
            })
        }

        fn touch(&mut self, key: &[u8]) {
            let pos = self.lru.iter().position(|k| k == key).unwrap();
            let k = self.lru.remove(pos).unwrap();
            self.lru.push_back(k);
        }

        fn drop_cold(&mut self, pick: impl Fn(&EntryState) -> bool) -> bool {
            let found = self
                .lru
                .iter()
                .take(RECLAIM_SCAN)
                .position(|k| pick(&self.states[k]));
            if let Some(pos) = found {
                let k = self.lru.remove(pos).unwrap();
                self.states.remove(&k);
            }
            found.is_some()
        }

        fn entry(&mut self, key: &[u8], now: u128) -> Option<&mut EntryState> {
            if self.states.contains_key(key) {
                self.touch(key);
            } else {
                if self.states.len() == self.limit
                    && !self.drop_cold(|e| e.tat <= now && e.holds == 0)
                    && (self.policy == FullPolicy::Deny || !self.drop_cold(|e| e.holds == 0))
                {
                    return None;
                }
                // A fresh entry is a full bucket: any arrival time not
                // after `now` will do, and the start of time is one.
                self.states.insert(key.to_vec(), EntryState::default());
                self.lru.push_back(key.to_vec());
            }
            self.states.get_mut(key)
        }

        fn take(&mut self, key: &[u8], now_ns: u64, cost: u64) -> Result<Decision, Error> {
            let (iv, tol) = (self.iv, self.tol);
            if u128::from(cost) > tol / iv {
                return Err(Error::CostExceedsBurst);
            }
            let now = self.at(now_ns);
            let e = self.entry(key, now).ok_or(Error::TableFull)?;
            let mut d = Decision::default();
            if let Some(next) = wide::take(e.tat, now, cost, iv, tol, &mut d) {
                e.tat = next;
            }
            Ok(d)
        }

        fn take_range(
            &mut self,
            key: &[u8],
            now_ns: u64,
            min: u64,
            max: u64,
        ) -> Result<Decision, Error> {
            let (iv, tol) = (self.iv, self.tol);
            let now = self.at(now_ns);
            let e = self.entry(key, now).ok_or(Error::TableFull)?;
            let mut d = Decision::default();
            if let Some((_, next)) = wide::range(e.tat, now, min, max, iv, tol, &mut d) {
                e.tat = next;
            }
            Ok(d)
        }

        fn hold(&mut self, key: &[u8], now_ns: u64, max: u32) -> Result<Decision, Error> {
            let (iv, tol) = (self.iv, self.tol);
            let now = self.at(now_ns);
            let e = self.entry(key, now).ok_or(Error::TableFull)?;
            if e.holds >= max {
                return Err(Error::HoldLimit);
            }
            let mut d = Decision::default();
            if let Some(next) = wide::take(e.tat, now, 1, iv, tol, &mut d) {
                e.tat = next;
                e.holds += 1;
            }
            Ok(d)
        }

        fn refund(&mut self, key: &[u8], now_ns: u64, tokens: u64) {
            let now = self.at(now_ns);
            let iv = self.iv;
            if let Some(e) = self.states.get_mut(key) {
                e.tat = wide::refund(e.tat, now, tokens, iv);
            }
        }
    }

    /// Nanoseconds in a Julian year.
    const YEAR: u64 = 31_557_600_000_000_000;

    /// How a reference-model run moves its clock.
    #[derive(Clone, Copy, Debug)]
    enum Uptime {
        /// From `start`, in steps of up to 200 ms.
        From(u64),
        /// From 10 s before the end of the epoch `start` is in, so the run
        /// (about 27 s of clock) crosses an epoch boundary with keys in
        /// debt.
        AcrossBoundary(u64),
        /// From `start`, now and then idling for up to three epochs.
        WithIdleJumps(u64),
    }

    /// Run one seeded script of 3,000 operations against the table and the
    /// model, checking every result and the touched key's state after each
    /// step.
    fn run_against_model(seed: u64, uptime: Uptime) {
        let mut rng = Rng(seed);
        let policy = if seed.is_multiple_of(2) {
            FullPolicy::Deny
        } else {
            FullPolicy::Evict
        };
        let limit = 1 + rng.below(40) as u32;
        let rate = Rate::new(1 + rng.below(20), SEC, 1 + rng.below(4));
        let t = Table::new(&config(limit, 1, rate, policy)).unwrap();
        let mut m = Model::new(limit, policy, rate);
        let nkeys = 1 + rng.below(3 * u64::from(limit));
        let (mut now_ns, jumps) = match uptime {
            Uptime::From(start) => (start, false),
            Uptime::AcrossBoundary(start) => {
                let end = (clock::epoch_of(start) + 1) << EPOCH_SHIFT;
                (end - 10 * SEC, false)
            }
            Uptime::WithIdleJumps(start) => (start, true),
        };
        for step_no in 0..3000 {
            let key = key_name(rng.below(nkeys) as u32);
            let key = key.as_bytes();
            let ctx = format!("seed {seed} {uptime:?} step {step_no} at {now_ns} ns");
            match rng.below(11) {
                0..=4 => {
                    let cost = rng.below(4);
                    assert_eq!(
                        t.take(key, now_ns, cost),
                        m.take(key, now_ns, cost),
                        "{ctx}"
                    );
                }
                5 => assert_eq!(t.hold(key, now_ns, 2), m.hold(key, now_ns, 2), "{ctx}"),
                6 => {
                    let want = match m.states.get_mut(key) {
                        Some(e) if e.holds > 0 => {
                            e.holds -= 1;
                            Ok(())
                        }
                        _ => Err(Error::NoSuchKey),
                    };
                    assert_eq!(t.release(key), want, "{ctx}");
                }
                7 => {
                    let want = match m.states.remove(key) {
                        Some(_) => {
                            m.lru.retain(|k| k != key);
                            Ok(())
                        }
                        None => Err(Error::NoSuchKey),
                    };
                    assert_eq!(t.remove(key), want, "{ctx}");
                }
                8 => {
                    let tokens = rng.below(3);
                    t.refund(key, now_ns, tokens);
                    m.refund(key, now_ns, tokens);
                }
                9 => {
                    let (min, max) = (rng.below(4), rng.below(5));
                    assert_eq!(
                        t.take_range(key, now_ns, min, max),
                        m.take_range(key, now_ns, min, max),
                        "{ctx}"
                    );
                }
                _ if jumps && rng.below(20) == 0 => {
                    now_ns = now_ns.saturating_add(rng.below(3 * EPOCH_NS));
                }
                _ => now_ns += rng.below(SEC / 5),
            }
            assert_eq!(t.get(key), m.get(key), "{ctx}");
        }
        t.check_invariants().unwrap();
        let st = t.stats();
        assert_eq!(st.entries, m.states.len() as u64, "seed {seed} {uptime:?}");
        let holds: u64 = m.states.values().map(|e| u64::from(e.holds)).sum();
        assert_eq!(st.holds, holds, "seed {seed} {uptime:?}");
        for k in m.lru.clone() {
            assert_eq!(t.get(&k), m.get(&k), "seed {seed} {uptime:?}");
        }
        // Sweeping far in the future leaves only the held keys.
        let held = m.states.values().filter(|e| e.holds > 0).count();
        t.sweep(now_ns + 100 * SEC, 1);
        assert_eq!(t.stats().entries, held as u64, "seed {seed} {uptime:?}");
    }

    #[test]
    fn matches_a_reference_model() {
        for seed in 0..40u64 {
            run_against_model(seed, Uptime::From(SEC));
        }
    }

    #[test]
    fn matches_the_reference_model_at_years_of_uptime() {
        // libpacer's table stopped refilling at 4.5 years; the epoch keeps
        // decisions exact at 4.5, 10 and 100 years, across epoch boundaries
        // with keys in debt, and after idling for up to three epochs.
        for start in [45 * YEAR / 10, 10 * YEAR, 100 * YEAR] {
            for seed in 0..12u64 {
                run_against_model(seed, Uptime::From(start));
                run_against_model(seed, Uptime::AcrossBoundary(start));
                run_against_model(seed, Uptime::WithIdleJumps(start));
            }
        }
    }

    #[test]
    fn keyed_limits_refill_past_libpacers_horizon() {
        // 10/s with a burst of 2, drained at 100 years: libpacer's clamp
        // would have frozen this key at 4.5 years and denied it forever.
        let t = make_table(100, 1, 2);
        let t0 = 100 * YEAR;
        assert!(t.take(b"k", t0, 2).unwrap().allowed);
        let d = t.take(b"k", t0, 1).unwrap();
        assert!(!d.allowed);
        // One token is 100 ms away.
        assert_eq!(d.retry_after_ns, SEC / 10);
        assert!(!t.take(b"k", t0 + SEC / 10 - 1, 1).unwrap().allowed);
        assert!(t.take(b"k", t0 + SEC / 10, 1).unwrap().allowed);
        let iv = u128::from(t.interval.load(Ordering::Relaxed));
        assert_eq!(t.get(b"k").unwrap().tat, clock::ticks_wide(t0) + 3 * iv);
        assert_eq!(lock(&t.shards[0]).base, clock::epoch_of(t0) - 1);
        // Every u64 timestamp works.
        assert!(t.take(b"k", u64::MAX - SEC, 2).unwrap().allowed);
        let d = t.take(b"k", u64::MAX - SEC + SEC / 20, 1).unwrap();
        assert_eq!((d.allowed, d.retry_after_ns), (false, SEC / 20));
        assert!(t.take(b"k", u64::MAX, 1).unwrap().allowed);
        t.check_invariants().unwrap();
    }

    #[test]
    fn debt_carries_across_a_rebase_exactly() {
        // 10/s, burst 5, drained 250 ms before the end of epoch 1; the next
        // timestamp is in epoch 2, which moves the base from 0 to 1.
        let t = make_table(100, 1, 5);
        let edge = 2 * EPOCH_NS;
        let t0 = edge - SEC / 4;
        assert!(t.take(b"a", t0, 5).unwrap().allowed);
        assert!(t.take(b"idle", SEC, 1).unwrap().allowed);
        assert_eq!(lock(&t.shards[0]).base, 0);
        // 300 ms later, across the edge: 3 tokens back, 2 short of a full
        // bucket, and the fourth is 100 ms away.
        let d = t.take(b"a", edge + SEC / 20, 4).unwrap();
        assert_eq!(
            (d.allowed, d.remaining, d.retry_after_ns),
            (false, 3, SEC / 10)
        );
        assert_eq!(lock(&t.shards[0]).base, 1);
        let iv = u128::from(t.interval.load(Ordering::Relaxed));
        assert_eq!(t.get(b"a").unwrap().tat, clock::ticks_wide(t0) + 5 * iv);
        // The key drained in epoch 0 is long idle: it reads as the start of
        // the new base epoch and still counts as a full bucket.
        let idle = t.get(b"idle").unwrap();
        assert_eq!(idle.tat, clock::epoch_start(1));
        let d = t.take(b"idle", edge, 1).unwrap();
        assert_eq!((d.allowed, d.remaining), (true, 4));
        t.check_invariants().unwrap();
    }

    #[test]
    fn a_shard_idle_for_a_century_catches_up_in_one_call() {
        let t = make_table(1000, 4, 3);
        for i in 0..200 {
            assert_eq!(take_key(&t, i, SEC), Ok(true));
        }
        let later = SEC + 100 * YEAR;
        // Every key is idle: a sweep removes them all, in every shard.
        assert_eq!(t.sweep(later, 4), 200);
        assert_eq!(t.stats().entries, 0);
        for p in t.shards.iter() {
            assert_eq!(lock(p).base, clock::epoch_of(later) - 1);
        }
        // And a key in debt that idles for a century comes back full.
        assert!(t.take(b"k", later, 3).unwrap().allowed);
        let much_later = later + 100 * YEAR;
        let d = t.take(b"k", much_later, 3).unwrap();
        assert_eq!((d.allowed, d.remaining), (true, 0));
        t.check_invariants().unwrap();
    }

    #[test]
    fn a_timestamp_before_the_base_epoch_counts_as_its_start() {
        // A caller more than an epoch behind breaks the contract; it is
        // served as if at the start of the base epoch, not wrapped.
        let t = make_table(100, 1, 1);
        let late = 10 * EPOCH_NS + SEC;
        assert!(t.take(b"k", late, 1).unwrap().allowed);
        assert_eq!(lock(&t.shards[0]).base, 9);
        let d = t.take(b"k", SEC, 1).unwrap();
        assert!(!d.allowed);
        // At the start of epoch 9 the key's next token is an epoch and a
        // bit away: one interval (100 ms) past the take at `late`.
        assert_eq!(d.retry_after_ns, late - 9 * EPOCH_NS + SEC / 10);
        // A refund of everything moves the arrival time back to that start.
        t.refund(b"k", SEC, u64::MAX);
        assert_eq!(t.get(b"k").unwrap().tat, clock::epoch_start(9));
        t.check_invariants().unwrap();
    }

    #[test]
    fn concurrent_takes_across_a_rebase_never_exceed_the_burst() {
        // Eight threads race on 64 keys, last written in epoch 0, at the
        // start of epoch 3: whichever thread moves each shard's base, every
        // key grants exactly its burst.
        for round in 0..10u64 {
            let t = Arc::new(make_table(1024, 4, 5));
            for i in 0..64 {
                assert!(t.take(key_name(i).as_bytes(), SEC, 5).unwrap().allowed);
            }
            let now = 3 * EPOCH_NS + round;
            let threads: Vec<_> = (0..8)
                .map(|n| {
                    let t = Arc::clone(&t);
                    std::thread::spawn(move || {
                        let mut granted = 0u64;
                        for i in 0..640u32 {
                            let k = key_name((i + n * 7) % 64);
                            granted += t.take(k.as_bytes(), now, 1).unwrap().granted;
                        }
                        granted
                    })
                })
                .collect();
            let total: u64 = threads.into_iter().map(|h| h.join().unwrap()).sum();
            assert_eq!(total, 64 * 5, "round {round}");
            t.check_invariants().unwrap();
            for p in t.shards.iter() {
                assert_eq!(lock(p).base, 2, "round {round}");
            }
        }
    }

    #[test]
    fn concurrent_takes_never_exceed_the_burst() {
        // Eight threads race on 64 keys with a burst of 5 at one instant:
        // exactly 5 grants per key, whatever the interleaving.
        let t = Arc::new(make_table(1024, 0, 5));
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let t = Arc::clone(&t);
                std::thread::spawn(move || {
                    let mut granted = 0u64;
                    for i in 0..640u32 {
                        let k = key_name((i + n * 7) % 64);
                        granted += t.take(k.as_bytes(), SEC, 1).unwrap().granted;
                    }
                    granted
                })
            })
            .collect();
        let total: u64 = threads.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(total, 64 * 5);
        t.check_invariants().unwrap();
        assert_eq!(t.stats().entries, 64);
    }

    #[test]
    fn concurrent_churn_keeps_the_table_consistent() {
        // Holds, releases, removes, sweeps and evicting inserts from several
        // threads; every admitted hold is released or removed, so the hold
        // count ends where the model says.
        let t = Arc::new(
            Table::new(&config(256, 4, Rate::new(1000, SEC, 4), FullPolicy::Evict)).unwrap(),
        );
        let threads: Vec<_> = (0..6u64)
            .map(|n| {
                let t = Arc::clone(&t);
                std::thread::spawn(move || {
                    let mut rng = Rng(n);
                    let mut now = SEC;
                    for _ in 0..4000 {
                        now += rng.below(SEC / 1000);
                        // Each thread owns its own keys for holds, so it can
                        // release exactly what it was granted.
                        let own = format!("t{n}-{}", rng.below(50));
                        match rng.below(6) {
                            0 => {
                                if t.hold(own.as_bytes(), now, 3).is_ok_and(|d| d.allowed) {
                                    t.release(own.as_bytes()).unwrap();
                                }
                            }
                            1 => {
                                let _ = t.remove(own.as_bytes());
                            }
                            2 => {
                                t.sweep(now, 1);
                            }
                            _ => {
                                let k = key_name(rng.below(1000) as u32);
                                let _ = t.take(k.as_bytes(), now, 1);
                            }
                        }
                    }
                })
            })
            .collect();
        for h in threads {
            h.join().unwrap();
        }
        t.check_invariants().unwrap();
        let st = t.stats();
        assert_eq!(st.holds, 0);
        assert!(st.entries <= 256);
        assert_eq!(
            st.inserts,
            st.entries + st.removals + st.reclaims + st.evictions
        );
    }
}
