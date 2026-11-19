//! The per-client connection quota: at most `limit` new connections from
//! one client in any window of an hour to 366 days, the TOML-only
//! `per_ip_conn_quota`. pacerd has no quota; this is edgefront's own, and
//! [`super::Admission`] checks it after the client table's hold.
//!
//! A [`QuotaTable`] keeps one [`Window`] of [`QUOTA_SLOTS`] buckets per
//! client, the same counter a [`crate::Quota`] uses, so a client's
//! decisions are exactly those of a `Quota` of its own. Like the client
//! table ([`crate::Table`]) its memory is allocated once and never grows:
//!
//! - It is split into the same number of shards as a client table of the
//!   same size, each behind its own lock, with the top bits of a key's
//!   SipHash-1-3 picking the shard.
//! - Each shard has a pool of exactly its share of entries, and an
//!   open-addressing index of `(tag, entry)` pairs at most three quarters
//!   full, with linear probing and backward-shift deletion. An entry is
//!   about 200 bytes, so the index moves 8-byte pairs and never the
//!   entries, which stay where they are on the shard's LRU list.
//! - When a new client arrives and its shard is full, the eight least
//!   recently used entries are scanned for an idle one, whose window is
//!   empty, and it is reclaimed. Otherwise [`FullPolicy::Deny`] refuses the
//!   new client and [`FullPolicy::Evict`] drops the least recently used,
//!   with its count.
//!
//! Every window's buckets start on multiples of the bucket width, so an
//! idle window is exactly a fresh one and reclaiming it changes no
//! decision. The bucket width is `window / 16`, so a count is within one
//! bucket (a sixteenth of the window) of the exact count, as for
//! [`crate::Quota`].

use std::fmt;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::ClientKey;
use crate::gcra::Error;
use crate::rate::QUOTA_SLOTS;
use crate::siphash::{KEY_LEN, KeyHash, random_key};
use crate::table::{FullPolicy, MAX_ENTRIES, TableStats, pick_shards};
use crate::window::Window;

const NIL: u32 = u32::MAX;
/// How many entries from the cold end of the LRU list an insert examines
/// looking for an idle one, as in [`crate::Table`].
const RECLAIM_SCAN: usize = 8;

/// One index slot: the low 32 bits of the key's hash, which give its home
/// slot, and the pool entry, or [`NIL`] for an empty slot.
#[derive(Clone, Copy)]
struct Slot {
    tag: u32,
    idx: u32,
}

impl Slot {
    const EMPTY: Slot = Slot { tag: 0, idx: NIL };
}

/// One client's window. A free entry is on the free list through `next`.
#[derive(Clone)]
struct Entry {
    key: ClientKey,
    tag: u32,
    /// LRU neighbours, as pool indexes in the same shard.
    prev: u32,
    next: u32,
    window: Window,
}

#[derive(Default)]
struct Stats {
    inserts: u64,
    removals: u64,
    reclaims: u64,
    evictions: u64,
    rejections: u64,
}

struct Shard {
    slots: Box<[Slot]>,
    /// Slot count - 1; the count is a power of two.
    mask: u32,
    pool: Box<[Entry]>,
    /// First free pool entry.
    free: u32,
    count: u32,
    /// Most recently used.
    head: u32,
    /// Least recently used.
    tail: u32,
    /// The window new entries get. Read and changed under the lock, so a
    /// reset never races an insert into keeping the old length.
    window_ns: u64,
    stats: Stats,
}

/// A shard behind its lock, alone on its cache lines.
#[repr(align(64))]
struct Padded(Mutex<Shard>);

#[inline]
fn lock(p: &Padded) -> MutexGuard<'_, Shard> {
    p.0.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shard {
    fn new(entries: u32, window_ns: u64) -> Self {
        let nslots = ((entries + 1) * 4).div_ceil(3).next_power_of_two();
        let blank = Entry {
            key: ClientKey {
                bytes: [0; super::CLIENT_KEY_MAX],
                len: 0,
            },
            tag: 0,
            prev: NIL,
            next: NIL,
            window: Window::new(window_ns, QUOTA_SLOTS, 0).expect("checked window"),
        };
        let mut s = Self {
            slots: vec![Slot::EMPTY; nslots as usize].into_boxed_slice(),
            mask: nslots - 1,
            pool: vec![blank; entries as usize].into_boxed_slice(),
            free: NIL,
            count: 0,
            head: NIL,
            tail: NIL,
            window_ns,
            stats: Stats::default(),
        };
        s.clear();
        s
    }

    #[inline]
    fn home(&self, tag: u32) -> u32 {
        tag & self.mask
    }

    /// Index of the key's slot, or of the empty slot that ends its probe
    /// chain. The index is at most three quarters full, so the loop ends.
    fn probe(&self, tag: u32, key: &ClientKey) -> u32 {
        let mut i = self.home(tag);
        loop {
            let s = self.slots[i as usize];
            if s.idx == NIL || (s.tag == tag && self.pool[s.idx as usize].key == *key) {
                return i;
            }
            i = (i + 1) & self.mask;
        }
    }

    #[inline]
    fn at(&mut self, idx: u32) -> &mut Entry {
        &mut self.pool[idx as usize]
    }

    fn lru_unlink(&mut self, idx: u32) {
        let (prev, next) = (self.at(idx).prev, self.at(idx).next);
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

    fn lru_push_head(&mut self, idx: u32) {
        let head = self.head;
        let e = self.at(idx);
        e.prev = NIL;
        e.next = head;
        if head != NIL {
            self.at(head).prev = idx;
        } else {
            self.tail = idx;
        }
        self.head = idx;
    }

    /// Delete the entry in slot `i`: back onto the free list, and later
    /// members of the probe chain shifted back into the hole, so lookups
    /// never meet tombstones. A slot at `j` may fill hole `i` only if its
    /// home is not in the cyclic range `(i, j]`.
    fn delete_slot(&mut self, mut i: u32) {
        let idx = self.slots[i as usize].idx;
        self.lru_unlink(idx);
        let free = self.free;
        self.at(idx).next = free;
        self.free = idx;
        let mut j = i;
        loop {
            j = (j + 1) & self.mask;
            let s = self.slots[j as usize];
            if s.idx == NIL {
                break;
            }
            let home = self.home(s.tag);
            let stays = if i <= j {
                i < home && home <= j
            } else {
                i < home || home <= j
            };
            if stays {
                continue;
            }
            self.slots[i as usize] = s;
            i = j;
        }
        self.slots[i as usize] = Slot::EMPTY;
        self.count -= 1;
    }

    fn delete_entry(&mut self, idx: u32) {
        let e = &self.pool[idx as usize];
        let i = self.probe(e.tag, &e.key);
        self.delete_slot(i);
    }

    /// Whether the entry's window is empty at `now_ns` and so is exactly a
    /// fresh entry's. A window ahead of `now_ns`, from a clock that went
    /// back, is not idle: a fresh window would start earlier.
    fn idle(&mut self, idx: u32, now_ns: u64) -> bool {
        let w = &mut self.at(idx).window;
        w.count(now_ns);
        w.total() == 0 && w.head_start() <= now_ns
    }

    /// Make room for one client in a full shard. Returns whether an entry
    /// was freed.
    fn make_room(&mut self, policy: FullPolicy, now_ns: u64) -> bool {
        let mut idx = self.tail;
        for _ in 0..RECLAIM_SCAN {
            if idx == NIL {
                break;
            }
            let prev = self.at(idx).prev;
            if self.idle(idx, now_ns) {
                self.delete_entry(idx);
                self.stats.reclaims += 1;
                return true;
            }
            idx = prev;
        }
        if policy != FullPolicy::Evict || self.tail == NIL {
            return false;
        }
        self.delete_entry(self.tail);
        self.stats.evictions += 1;
        true
    }

    fn take(
        &mut self,
        policy: FullPolicy,
        key: &ClientKey,
        tag: u32,
        now_ns: u64,
        limit: u64,
    ) -> Result<bool, Error> {
        let mut i = self.probe(tag, key);
        let idx = match self.slots[i as usize].idx {
            NIL => {
                if self.count as usize == self.pool.len() {
                    if !self.make_room(policy, now_ns) {
                        self.stats.rejections += 1;
                        return Err(Error::TableFull);
                    }
                    // Deleting shifted slots around; find the hole again.
                    i = self.probe(tag, key);
                }
                let idx = self.free;
                let window_ns = self.window_ns;
                let slot_ns = window_ns / u64::from(QUOTA_SLOTS);
                let e = self.at(idx);
                let next_free = e.next;
                e.key = *key;
                e.tag = tag;
                e.window = Window::new(window_ns, QUOTA_SLOTS, now_ns - now_ns % slot_ns)
                    .expect("checked window");
                self.free = next_free;
                self.slots[i as usize] = Slot { tag, idx };
                self.lru_push_head(idx);
                self.count += 1;
                self.stats.inserts += 1;
                idx
            }
            idx => {
                if self.head != idx {
                    self.lru_unlink(idx);
                    self.lru_push_head(idx);
                }
                idx
            }
        };
        let w = &mut self.at(idx).window;
        if w.count(now_ns) >= limit {
            return Ok(false);
        }
        w.add(now_ns, 1);
        Ok(true)
    }

    fn count_of(&mut self, key: &ClientKey, tag: u32, now_ns: u64) -> Option<u64> {
        let idx = self.slots[self.probe(tag, key) as usize].idx;
        (idx != NIL).then(|| self.at(idx).window.count(now_ns))
    }

    /// Drop every entry, counting them as removals.
    fn clear(&mut self) {
        self.stats.removals += u64::from(self.count);
        self.slots.fill(Slot::EMPTY);
        let n = self.pool.len() as u32;
        for (k, e) in self.pool.iter_mut().enumerate() {
            let k = k as u32;
            e.next = if k + 1 < n { k + 1 } else { NIL };
        }
        self.free = if n > 0 { 0 } else { NIL };
        self.count = 0;
        self.head = NIL;
        self.tail = NIL;
    }

    /// Remove every idle entry in one pass and return how many went.
    fn sweep(&mut self, now_ns: u64) -> usize {
        let mut removed = 0;
        let mut i = 0;
        while i <= self.mask {
            let idx = self.slots[i as usize].idx;
            if idx != NIL && self.idle(idx, now_ns) {
                // Another slot may shift into slot i; look at it again.
                self.delete_slot(i);
                removed += 1;
                continue;
            }
            i += 1;
        }
        self.stats.removals += removed as u64;
        removed
    }

    fn check(&self) -> Result<(), &'static str> {
        let mut live = vec![false; self.pool.len()];
        let mut occupied = 0u32;
        for (i, s) in self.slots.iter().enumerate() {
            if s.idx == NIL {
                continue;
            }
            occupied += 1;
            let Some(e) = self.pool.get(s.idx as usize) else {
                return Err("an index slot points outside the pool");
            };
            if std::mem::replace(&mut live[s.idx as usize], true) {
                return Err("two index slots point at one entry");
            }
            if e.tag != s.tag || self.probe(e.tag, &e.key) as usize != i {
                return Err("a client is not found by a probe from its home slot");
            }
        }
        let (mut linked, mut prev, mut idx) = (0u32, NIL, self.head);
        while idx != NIL {
            let e = &self.pool[idx as usize];
            linked += 1;
            if !live[idx as usize] || e.prev != prev || linked > occupied {
                return Err("the LRU list is not linked both ways over live entries");
            }
            prev = idx;
            idx = e.next;
        }
        let mut free = 0usize;
        let mut idx = self.free;
        while idx != NIL {
            if live[idx as usize] || free >= self.pool.len() {
                return Err("the free list holds a live entry or loops");
            }
            free += 1;
            idx = self.pool[idx as usize].next;
        }
        if prev != self.tail || linked != occupied || occupied != self.count {
            return Err("the LRU list, the index and the count disagree");
        }
        if free + occupied as usize != self.pool.len() {
            return Err("the free list and the live entries do not cover the pool");
        }
        Ok(())
    }
}

/// A fixed-memory, sharded table of per-client connection quotas. Every
/// method is thread-safe. See the [module docs](self).
pub struct QuotaTable {
    shards: Box<[Padded]>,
    /// The top bits of a hash pick the shard; 64 for a single shard.
    shard_shift: u32,
    capacity: u32,
    policy: FullPolicy,
    limit: AtomicU64,
    window_ns: AtomicU64,
    sweep_cursor: AtomicU32,
    memory: usize,
    hash_key: [u8; KEY_LEN],
}

impl fmt::Debug for QuotaTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuotaTable")
            .field("shards", &self.shards.len())
            .field("capacity", &self.capacity)
            .field("policy", &self.policy)
            .field("limit", &self.limit())
            .field("window_ns", &self.window_ns())
            .finish_non_exhaustive()
    }
}

impl QuotaTable {
    /// Check a quota of `limit` per `window_ns`: [`Error::InvalidRate`] for
    /// a limit of 0, [`Error::InvalidWindow`] for a window under
    /// [`QUOTA_SLOTS`] nanoseconds, as [`crate::Quota::new`] refuses them.
    pub fn check(limit: u64, window_ns: u64) -> Result<(), Error> {
        if limit == 0 {
            return Err(Error::InvalidRate);
        }
        Window::new(window_ns, QUOTA_SLOTS, 0).map(drop)
    }

    /// A table for up to `max_entries` clients, each allowed `limit` new
    /// connections in any `window_ns`, with `policy` for a new client when
    /// its shard is full. All of its memory is allocated here. Fails with
    /// [`Error::InvalidConfig`] for a size of 0 or over
    /// [`MAX_ENTRIES`], and as [`QuotaTable::check`] does.
    pub fn new(
        max_entries: u32,
        limit: u64,
        window_ns: u64,
        policy: FullPolicy,
    ) -> Result<Self, Error> {
        if max_entries == 0 || max_entries > MAX_ENTRIES {
            return Err(Error::InvalidConfig);
        }
        Self::check(limit, window_ns)?;
        let nshards = pick_shards(max_entries);
        let base = max_entries / nshards;
        let extra = max_entries % nshards;
        let shards: Box<[Padded]> = (0..nshards)
            .map(|i| {
                Padded(Mutex::new(Shard::new(
                    base + u32::from(i < extra),
                    window_ns,
                )))
            })
            .collect();
        let slots: usize = shards.iter().map(|p| lock(p).slots.len()).sum();
        let memory = std::mem::size_of::<Self>()
            + std::mem::size_of::<Padded>() * shards.len()
            + std::mem::size_of::<Slot>() * slots
            + std::mem::size_of::<Entry>() * max_entries as usize;
        Ok(Self {
            shards,
            shard_shift: 64 - nshards.trailing_zeros(),
            capacity: max_entries,
            policy,
            limit: AtomicU64::new(limit),
            window_ns: AtomicU64::new(window_ns),
            sweep_cursor: AtomicU32::new(0),
            memory,
            hash_key: random_key(),
        })
    }

    #[inline]
    fn lock(&self, key: &ClientKey) -> (u32, MutexGuard<'_, Shard>) {
        let hash = KeyHash::Sip13.hash(&self.hash_key, key.as_bytes());
        // A single shard has a shift of 64, which checked_shr maps to 0.
        let idx = hash.checked_shr(self.shard_shift).unwrap_or(0) as usize;
        (hash as u32, lock(&self.shards[idx]))
    }

    /// Count one new connection from `key` at `now_ns` if its window has
    /// room: `Ok(true)` when it was counted, `Ok(false)` when the client
    /// already has `limit` in its window, which counts nothing. A new
    /// client is added, or refused with [`Error::TableFull`] when its
    /// shard is full and nothing can be reclaimed or evicted.
    pub fn take(&self, key: &ClientKey, now_ns: u64) -> Result<bool, Error> {
        let limit = self.limit.load(Ordering::Relaxed);
        let (tag, mut s) = self.lock(key);
        s.take(self.policy, key, tag, now_ns, limit)
    }

    /// The client's count at `now_ns`, without adding it or changing its
    /// recency, or `None` for a client the table does not track.
    pub fn count(&self, key: &ClientKey, now_ns: u64) -> Option<u64> {
        let (tag, mut s) = self.lock(key);
        s.count_of(key, tag, now_ns)
    }

    /// Change the limit for every client, keeping their counts. A limit of
    /// 0 is refused with [`Error::InvalidRate`].
    pub fn set_limit(&self, limit: u64) -> Result<(), Error> {
        Self::check(limit, self.window_ns())?;
        self.limit.store(limit, Ordering::Relaxed);
        Ok(())
    }

    /// Drop every client and give new ones windows of `window_ns`, after
    /// [`QuotaTable::check`]ing it with the current limit. A window of a
    /// different length has buckets of a different width, so counts
    /// cannot carry over. Shards are cleared one at a time, so a take
    /// running meanwhile counts against whichever window its shard has.
    pub fn reset(&self, window_ns: u64) -> Result<(), Error> {
        Self::check(self.limit(), window_ns)?;
        self.window_ns.store(window_ns, Ordering::Relaxed);
        for p in &self.shards {
            let mut s = lock(p);
            s.window_ns = window_ns;
            s.clear();
        }
        Ok(())
    }

    /// The most new connections per client in one window.
    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }

    /// The window's length in nanoseconds.
    pub fn window_ns(&self) -> u64 {
        self.window_ns.load(Ordering::Relaxed)
    }

    /// Remove clients whose windows are empty from the next `nshards`
    /// shards (round robin across calls) and return how many were
    /// removed. Optional, as for [`crate::Table::sweep`]: inserts reclaim
    /// idle clients on their own.
    pub fn sweep(&self, now_ns: u64, nshards: u32) -> usize {
        let n = self.shards.len();
        let mut removed = 0;
        for _ in 0..(nshards as usize).min(n) {
            let idx = self.sweep_cursor.fetch_add(1, Ordering::Relaxed) as usize;
            removed += lock(&self.shards[idx & (n - 1)]).sweep(now_ns);
        }
        removed
    }

    /// A snapshot of the counters, summed over shards, in the client
    /// table's form; `holds` is always 0.
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

    /// Walk every shard under its lock and verify the index, the LRU list,
    /// the free list and the count agree. For tests; linear in the size.
    pub fn check_invariants(&self) -> Result<(), &'static str> {
        self.shards.iter().try_for_each(|p| lock(p).check())
    }
}

#[cfg(test)]
mod tests;
