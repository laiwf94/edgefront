//! The disk tier as a log-structured segment store.
//!
//! Every write is appended to the active segment, a file under
//! `segments/` named by its sequence number. A segment that reaches
//! [`SegmentStore::segment_bytes`] is sealed and a new one started. Each
//! record is framed:
//!
//! ```text
//! magic   4 bytes  "EFS1"
//! kind    1 byte   1 = put, 2 = delete
//! len     u32      body length, big-endian
//! sum     16 bytes the first 16 bytes of SHA-256 over kind, len and body
//! body    len bytes a put's codec record (crate::store::codec, its own
//!                   checksums inside), or a delete's key
//! ```
//!
//! An in-memory index maps each key to the place of its newest put. A
//! delete (a removal or a purge) appends a tombstone, so a restart that
//! replays the log does not bring the object back.
//!
//! **Space.** The segments' total size is held to the store's capacity:
//! when a write would pass it, the oldest segment is deleted with every
//! object still in it, which is FIFO eviction a segment at a time.
//! Replacements and deletes leave dead records behind; [`SegmentStore::compact`]
//! rewrites the oldest segment's live records at the head of the log and
//! deletes it, once less than half of it is live. Compacting only the oldest
//! segment means no older segment is left for its tombstones to guard
//! against, so they are dropped. `written_bytes` against `user_bytes` is the
//! write amplification.
//!
//! **Crashes.** A torn write leaves a frame at a segment's end that is
//! short or fails its checksum. The scan at startup stops at the first such
//! frame and truncates the segment there. Reads check the frame checksum
//! and the record's own checksums again, and a record that fails them is
//! dropped, so a corrupt object is never served.
//!
//! **Warm restart.** On a clean shutdown [`SegmentStore::shutdown`] writes
//! the index to `index.snap` ([`super::index`]). The next start loads it,
//! checks its checksum and that every segment it names is still at least as
//! long, and replays only what was written after it. The snapshot is
//! deleted once loaded, so after a crash the store falls back to a full scan
//! instead of trusting an index that no longer matches the log.
//!
//! All methods block; async callers run them on the blocking pool.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use sha2::{Digest, Sha256};

use super::codec;
use super::index::{self, Snapshot};
use crate::object::{Entry, Key, surrogate_keys};

/// The frame magic.
pub const FRAME_MAGIC: &[u8; 4] = b"EFS1";
/// Bytes before a frame's body.
pub const FRAME_HEADER: usize = 4 + 1 + 4 + 16;
const PUT: u8 = 1;
const DELETE: u8 = 2;
/// The largest segment, unless a single record is larger.
pub const MAX_SEGMENT: u64 = 64 << 20;
/// Compaction rewrites the oldest segment once less than this share of it
/// is live.
pub const COMPACT_BELOW: f64 = 0.5;

/// Where a record lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loc {
    /// The segment's sequence number.
    pub segment: u64,
    /// The frame's offset in it.
    pub offset: u64,
    /// The frame's length, header included.
    pub frame: u64,
    /// The record's surrogate keys, for purges.
    pub tags: Vec<Arc<str>>,
}

#[derive(Default)]
struct Index {
    map: HashMap<Key, Loc>,
    tags: HashMap<Arc<str>, HashSet<Key>>,
    /// Bytes of live frames.
    live: u64,
    /// Per segment: its length and its live bytes.
    segments: BTreeMap<u64, Segment>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) len: u64,
    pub(crate) live: u64,
}

/// The log-structured disk tier.
pub struct SegmentStore {
    dir: PathBuf,
    capacity: u64,
    segment_bytes: u64,
    index: Mutex<Index>,
    /// Serialises appends, rotation and compaction, and holds the active
    /// segment's file open.
    writer: Mutex<Option<(u64, File)>>,
    /// Records found corrupt or foreign and dropped, at startup or on read.
    pub dropped: AtomicU64,
    /// Bytes of records callers asked to store.
    pub user_bytes: AtomicU64,
    /// Bytes of frames written: puts, tombstones and compaction's copies.
    pub written_bytes: AtomicU64,
    /// Bytes copied by compaction.
    pub compacted_bytes: AtomicU64,
    /// Segments deleted to keep the capacity.
    pub evicted_segments: AtomicU64,
    /// Whether the last start loaded a snapshot.
    pub warm: bool,
}

fn frame_sum(kind: u8, body: &[u8]) -> [u8; 16] {
    let mut h = Sha256::new();
    h.update([kind]);
    h.update((body.len() as u32).to_be_bytes());
    h.update(body);
    let digest = h.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER + body.len());
    out.extend_from_slice(FRAME_MAGIC);
    out.push(kind);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&frame_sum(kind, body));
    out.extend_from_slice(body);
    out
}

/// A frame's header, checked for magic and kind.
fn parse_header(bytes: &[u8; FRAME_HEADER]) -> Option<(u8, u64, [u8; 16])> {
    if &bytes[..4] != FRAME_MAGIC || !matches!(bytes[4], PUT | DELETE) {
        return None;
    }
    let len = u32::from_be_bytes(bytes[5..9].try_into().ok()?) as u64;
    let mut sum = [0u8; 16];
    sum.copy_from_slice(&bytes[9..25]);
    Some((bytes[4], len, sum))
}

fn segment_name(n: u64) -> String {
    format!("{n:016x}.seg")
}

fn segment_number(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let hex = name.strip_suffix(".seg")?;
    (hex.len() == 16)
        .then(|| u64::from_str_radix(hex, 16).ok())
        .flatten()
}

impl SegmentStore {
    /// Opens `dir`, creating it if needed: loads the snapshot if there is a
    /// valid one and replays what came after it, or scans every segment.
    /// Files of the earlier one-file-per-URL layout are removed.
    pub fn open(dir: &Path, capacity: u64) -> io::Result<Self> {
        Self::open_with(dir, capacity, (capacity / 8).clamp(1 << 20, MAX_SEGMENT))
    }

    /// As [`SegmentStore::open`] with a given segment size, for tests.
    pub fn open_with(dir: &Path, capacity: u64, segment_bytes: u64) -> io::Result<Self> {
        let segments_dir = dir.join("segments");
        fs::create_dir_all(&segments_dir)?;
        let mut store = SegmentStore {
            dir: dir.to_path_buf(),
            capacity,
            segment_bytes: segment_bytes.max(FRAME_HEADER as u64 + 1),
            index: Mutex::new(Index::default()),
            writer: Mutex::new(None),
            dropped: AtomicU64::new(0),
            user_bytes: AtomicU64::new(0),
            written_bytes: AtomicU64::new(0),
            compacted_bytes: AtomicU64::new(0),
            evicted_segments: AtomicU64::new(0),
            warm: false,
        };
        store.remove_old_layout()?;
        let mut on_disk: BTreeMap<u64, u64> = BTreeMap::new();
        for item in fs::read_dir(&segments_dir)? {
            let item = item?;
            let path = item.path();
            match segment_number(&path) {
                Some(n) if item.file_type()?.is_file() => {
                    on_disk.insert(n, item.metadata()?.len());
                }
                _ => {}
            }
        }
        let snapshot_path = dir.join(index::SNAPSHOT);
        let snapshot = match fs::read(&snapshot_path) {
            Ok(bytes) => {
                let _ = fs::remove_file(&snapshot_path);
                Snapshot::decode(&bytes).filter(|s| s.covers(&on_disk))
            }
            Err(_) => None,
        };
        let mut index = Index::default();
        let replay_from: BTreeMap<u64, u64> = match snapshot {
            Some(snap) => {
                store.warm = true;
                for (n, seg) in &snap.segments {
                    index.segments.insert(*n, *seg);
                }
                for (key, loc) in snap.entries {
                    index_put(&mut index, key, loc);
                }
                on_disk
                    .iter()
                    .map(|(&n, _)| (n, snap.segments.get(&n).map_or(0, |s| s.len)))
                    .collect()
            }
            None => on_disk.keys().map(|&n| (n, 0)).collect(),
        };
        for (n, from) in replay_from {
            store.replay(&mut index, n, from)?;
        }
        *store
            .index
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner) = index;
        {
            let _w = store.writer.lock().unwrap_or_else(PoisonError::into_inner);
            store.evict_to_capacity(0);
        }
        Ok(store)
    }

    fn remove_old_layout(&self) -> io::Result<()> {
        let _ = fs::remove_dir_all(self.dir.join("tmp"));
        for item in fs::read_dir(&self.dir)? {
            let path = item?.path();
            if path.extension().is_some_and(|e| e == "obj") && path.is_file() {
                fs::remove_file(&path)?;
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn segment_path(&self, n: u64) -> PathBuf {
        self.dir.join("segments").join(segment_name(n))
    }

    /// Replays segment `n` from `from`, truncating it at the first frame
    /// that is short or fails its checksum.
    fn replay(&self, index: &mut Index, n: u64, from: u64) -> io::Result<()> {
        let path = self.segment_path(n);
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let len = file.metadata()?.len();
        let mut offset = from;
        file.seek(SeekFrom::Start(offset))?;
        let mut reader = io::BufReader::with_capacity(1 << 20, &mut file);
        let good_end = loop {
            if offset == len {
                break offset;
            }
            let mut head = [0u8; FRAME_HEADER];
            if reader.read_exact(&mut head).is_err() {
                break offset;
            }
            let Some((kind, body_len, sum)) = parse_header(&head) else {
                break offset;
            };
            if offset + FRAME_HEADER as u64 + body_len > len {
                break offset;
            }
            let mut body = vec![0u8; body_len as usize];
            if reader.read_exact(&mut body).is_err() || frame_sum(kind, &body) != sum {
                break offset;
            }
            let frame_len = FRAME_HEADER as u64 + body_len;
            match kind {
                PUT => match codec::decode_header(&body) {
                    Ok(header) => {
                        let tags = tags_of(&header);
                        let loc = Loc {
                            segment: n,
                            offset,
                            frame: frame_len,
                            tags,
                        };
                        index_put(index, header.key, loc);
                    }
                    Err(_) => {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                },
                _ => {
                    if let Ok(key) = std::str::from_utf8(&body) {
                        index_remove(index, key);
                    }
                }
            }
            offset += frame_len;
        };
        drop(reader);
        if good_end < len {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            file.set_len(good_end)?;
        }
        index.segments.entry(n).or_default().len = good_end;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Index> {
        self.index.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The segment size this store rotates at.
    pub fn segment_bytes(&self) -> u64 {
        self.segment_bytes
    }

    /// The segments' total size on disk.
    pub fn disk_bytes(&self) -> u64 {
        self.lock().segments.values().map(|s| s.len).sum()
    }

    /// How many segments there are.
    pub fn segments(&self) -> usize {
        self.lock().segments.len()
    }

    /// `written_bytes / user_bytes`, or 0 before any write.
    pub fn write_amplification(&self) -> f64 {
        let user = self.user_bytes.load(Ordering::Relaxed);
        if user == 0 {
            0.0
        } else {
            self.written_bytes.load(Ordering::Relaxed) as f64 / user as f64
        }
    }

    /// Appends one frame, rotating first if it would not fit in the active
    /// segment, and syncs it to disk if `sync`. Returns where it went.
    /// `active` is the writer lock's open file.
    fn append(
        &self,
        active: &mut Option<(u64, File)>,
        kind: u8,
        body: &[u8],
        sync: bool,
    ) -> io::Result<(u64, u64, u64)> {
        let framed = frame(kind, body);
        let len = framed.len() as u64;
        let (newest, used) = {
            let index = self.lock();
            match index.segments.last_key_value() {
                Some((&n, seg)) => (n, seg.len),
                None => (0, u64::MAX),
            }
        };
        let (segment, offset) = if used == u64::MAX || (used > 0 && used + len > self.segment_bytes)
        {
            let next = if used == u64::MAX { 0 } else { newest + 1 };
            if let Some((_, file)) = active.take() {
                file.sync_data()?;
            }
            File::create(self.segment_path(next))?;
            self.lock().segments.insert(next, Segment::default());
            (next, 0)
        } else {
            (newest, used)
        };
        if active.as_ref().is_none_or(|(n, _)| *n != segment) {
            let file = OpenOptions::new()
                .append(true)
                .open(self.segment_path(segment))?;
            *active = Some((segment, file));
        }
        let (_, file) = active.as_mut().expect("opened above");
        file.write_all(&framed)?;
        if sync {
            file.sync_data()?;
        }
        self.written_bytes.fetch_add(len, Ordering::Relaxed);
        self.lock().segments.entry(segment).or_default().len = offset + len;
        Ok((segment, offset, len))
    }

    /// Deletes the oldest segments until `extra` more bytes fit in the
    /// capacity, never the active one. Call with the writer lock held.
    fn evict_to_capacity(&self, extra: u64) {
        loop {
            let victim = {
                let index = self.lock();
                let total: u64 = index.segments.values().map(|s| s.len).sum();
                if total + extra <= self.capacity || index.segments.len() <= 1 {
                    return;
                }
                *index.segments.keys().next().expect("more than one segment")
            };
            self.drop_segment(victim);
            self.evicted_segments.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Removes segment `n` and every object indexed in it.
    fn drop_segment(&self, n: u64) {
        let mut index = self.lock();
        let keys: Vec<Key> = index
            .map
            .iter()
            .filter(|(_, loc)| loc.segment == n)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            index_remove(&mut index, &key);
        }
        index.segments.remove(&n);
        drop(index);
        let _ = fs::remove_file(self.segment_path(n));
    }

    /// Writes an entry and syncs it. Returns false if it was too large to
    /// keep.
    pub fn put(&self, key: &Key, entry: &Entry) -> io::Result<bool> {
        Ok(self.put_many(std::iter::once((key.clone(), entry)))? == 1)
    }

    /// Writes entries with one sync at the end. Returns how many were kept;
    /// one too large to keep is skipped.
    pub fn put_many<'e>(
        &self,
        entries: impl IntoIterator<Item = (Key, &'e Entry)>,
    ) -> io::Result<usize> {
        let mut active = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let mut kept = 0;
        for (key, entry) in entries {
            let Some(record) = codec::encode(&key, entry) else {
                continue;
            };
            let frame_len = (FRAME_HEADER + record.len()) as u64;
            if frame_len > self.capacity || record.len() > u32::MAX as usize {
                continue;
            }
            self.evict_to_capacity(frame_len);
            let (segment, offset, frame) = self.append(&mut active, PUT, &record, false)?;
            self.user_bytes
                .fetch_add(record.len() as u64, Ordering::Relaxed);
            let loc = Loc {
                segment,
                offset,
                frame,
                tags: entry.tags(),
            };
            index_put(&mut self.lock(), key, loc);
            // The new segment may have pushed the total over.
            self.evict_to_capacity(0);
            kept += 1;
        }
        if let Some((_, file)) = active.as_ref() {
            file.sync_data()?;
        }
        Ok(kept)
    }

    /// Reads the frame at `loc` and checks it.
    fn read_frame(&self, loc: &Loc) -> Option<Vec<u8>> {
        let mut file = File::open(self.segment_path(loc.segment)).ok()?;
        file.seek(SeekFrom::Start(loc.offset)).ok()?;
        let mut buf = vec![0u8; loc.frame as usize];
        file.read_exact(&mut buf).ok()?;
        let head: &[u8; FRAME_HEADER] = buf[..FRAME_HEADER].try_into().ok()?;
        let (kind, len, sum) = parse_header(head)?;
        let body = buf.split_off(FRAME_HEADER);
        (kind == PUT && len == body.len() as u64 && frame_sum(kind, &body) == sum).then_some(body)
    }

    /// Reads an entry. A record that fails its checks is dropped.
    pub fn get(&self, key: &str) -> Option<Entry> {
        let loc = self.lock().map.get(key).cloned()?;
        let Some(body) = self.read_frame(&loc) else {
            // Gone with its segment, or damaged: either way not served.
            if self.lock().segments.contains_key(&loc.segment) {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            self.forget(key, &loc);
            return None;
        };
        match codec::decode(&body) {
            Ok((stored_key, entry)) if &*stored_key == key => Some(entry),
            _ => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.forget(key, &loc);
                None
            }
        }
    }

    /// Drops `key` from the index if it still points at `loc`.
    fn forget(&self, key: &str, loc: &Loc) {
        let mut index = self.lock();
        if index.map.get(key) == Some(loc) {
            index_remove(&mut index, key);
        }
    }

    /// Removes an entry, appending a tombstone so a restart does not bring
    /// it back.
    pub fn remove(&self, key: &str) -> bool {
        let mut active = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        if !index_remove(&mut self.lock(), key) {
            return false;
        }
        // Without the tombstone the object would come back after a crash;
        // that costs a stale hit at worst, never a corrupt one.
        let _ = self.append(&mut active, DELETE, key.as_bytes(), true);
        true
    }

    /// Removes every entry carrying surrogate key `tag`.
    pub fn purge_tag(&self, tag: &str) -> usize {
        let keys: Vec<Key> = self
            .lock()
            .tags
            .get(tag)
            .map(|k| k.iter().cloned().collect())
            .unwrap_or_default();
        keys.iter().filter(|k| self.remove(k)).count()
    }

    /// Rewrites the oldest sealed segment if less than [`COMPACT_BELOW`] of
    /// it is live: its live records are appended at the head and it is
    /// deleted. Returns whether it compacted.
    pub fn compact(&self) -> io::Result<bool> {
        let mut active = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let (oldest, keys) = {
            let index = self.lock();
            if index.segments.len() < 2 {
                return Ok(false);
            }
            let (&n, seg) = index.segments.iter().next().expect("two segments");
            if seg.len > 0 && (seg.live as f64) >= COMPACT_BELOW * seg.len as f64 {
                return Ok(false);
            }
            let keys: Vec<(Key, Loc)> = index
                .map
                .iter()
                .filter(|(_, loc)| loc.segment == n)
                .map(|(k, l)| (k.clone(), l.clone()))
                .collect();
            (n, keys)
        };
        for (key, loc) in keys {
            let Some(body) = self.read_frame(&loc) else {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.forget(&key, &loc);
                continue;
            };
            let (segment, offset, frame) = self.append(&mut active, PUT, &body, false)?;
            self.compacted_bytes.fetch_add(frame, Ordering::Relaxed);
            let mut index = self.lock();
            if index.map.get(&key) == Some(&loc) {
                index_put(
                    &mut index,
                    key,
                    Loc {
                        segment,
                        offset,
                        frame,
                        tags: loc.tags,
                    },
                );
            }
        }
        // The copies are durable before the original goes.
        if let Some((_, file)) = active.as_ref() {
            file.sync_data()?;
        }
        self.drop_segment(oldest);
        Ok(true)
    }

    /// Writes the index snapshot, for a warm restart. Call on a clean
    /// shutdown, after the last write.
    pub fn shutdown(&self) -> io::Result<()> {
        let _w = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let index = self.lock();
        let bytes = index::encode(&index.map, &index.segments);
        drop(index);
        let temp = self.dir.join(format!("{}.tmp", index::SNAPSHOT));
        let mut file = File::create(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, self.dir.join(index::SNAPSHOT))
    }

    /// Bytes of live records.
    pub fn bytes(&self) -> u64 {
        self.lock().live
    }

    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn tags_of(header: &codec::RecordHeader) -> Vec<Arc<str>> {
    let mut tags: Vec<Arc<str>> = Vec::new();
    for v in &header.variants {
        for t in surrogate_keys(&v.headers) {
            if !tags.contains(&t) {
                tags.push(t);
            }
        }
    }
    tags
}

fn index_put(index: &mut Index, key: Key, loc: Loc) {
    index_remove(index, &key);
    for tag in &loc.tags {
        index
            .tags
            .entry(tag.clone())
            .or_default()
            .insert(key.clone());
    }
    index.live += loc.frame;
    index.segments.entry(loc.segment).or_default().live += loc.frame;
    index.map.insert(key, loc);
}

fn index_remove(index: &mut Index, key: &str) -> bool {
    let Some((key, loc)) = index.map.remove_entry(key) else {
        return false;
    };
    index.live -= loc.frame;
    if let Some(seg) = index.segments.get_mut(&loc.segment) {
        seg.live = seg.live.saturating_sub(loc.frame);
    }
    for tag in &loc.tags {
        if let Some(keys) = index.tags.get_mut(tag) {
            keys.remove(&key);
            if keys.is_empty() {
                index.tags.remove(tag);
            }
        }
    }
    true
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::object::tests::object;

    /// A fresh directory under the system temp dir, removed on drop.
    pub(crate) struct TempDir(pub PathBuf);

    impl TempDir {
        pub(crate) fn new(name: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("cachet-{name}-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn entry(tags: &str, body: &'static str) -> Entry {
        Entry::single(object(&[("surrogate-key", tags)], &[], body))
    }

    fn frame_len(key: &str, body: &'static str) -> u64 {
        frame_len_tagged(key, "", body)
    }

    fn frame_len_tagged(key: &str, tags: &str, body: &'static str) -> u64 {
        (FRAME_HEADER + codec::encode(key, &entry(tags, body)).unwrap().len()) as u64
    }

    fn only_segment(dir: &TempDir) -> PathBuf {
        let mut segs: Vec<PathBuf> = fs::read_dir(dir.0.join("segments"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        segs.sort();
        segs.pop().unwrap()
    }

    #[test]
    fn put_get_and_reopen() {
        let dir = TempDir::new("seg-reopen");
        {
            let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
            assert!(store.is_empty());
            assert!(store.put(&"h/a".into(), &entry("t1", "alpha")).unwrap());
            assert!(store.put(&"h/b".into(), &entry("t1 t2", "beta")).unwrap());
            assert_eq!(store.get("h/a").unwrap().variants[0].body, "alpha");
            assert!(store.get("h/missing").is_none());
        }
        // No snapshot: a full scan.
        let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
        assert!(!store.warm);
        assert_eq!(store.len(), 2);
        assert_eq!(store.get("h/b").unwrap().variants[0].body, "beta");
        assert_eq!(store.purge_tag("t1"), 2);
        assert!(store.is_empty());
        assert_eq!(store.bytes(), 0);
        // The tombstones keep them gone after a restart.
        drop(store);
        let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn replacing_a_key_keeps_the_newest() {
        let dir = TempDir::new("seg-replace");
        let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
        store.put(&"h/a".into(), &entry("t1", "one")).unwrap();
        store.put(&"h/a".into(), &entry("t2", "two")).unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(store.get("h/a").unwrap().variants[0].body, "two");
        assert_eq!(store.purge_tag("t1"), 0);
        drop(store);
        let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
        assert_eq!(store.get("h/a").unwrap().variants[0].body, "two");
        assert_eq!(store.purge_tag("t2"), 1);
        assert!(!store.remove("h/a"));
    }

    #[test]
    fn the_capacity_evicts_whole_segments_oldest_first() {
        let dir = TempDir::new("seg-fifo");
        let one = frame_len("h/00", "x");
        // Two records a segment, room for three segments.
        let store = SegmentStore::open_with(&dir.0, 6 * one, 2 * one).unwrap();
        for i in 0..20 {
            store
                .put(&format!("h/{i:02}").into(), &entry("", "x"))
                .unwrap();
            assert!(
                store.disk_bytes() <= 6 * one,
                "{} > {}",
                store.disk_bytes(),
                6 * one
            );
        }
        assert!(store.get("h/00").is_none());
        assert!(store.get("h/19").is_some());
        assert!(store.len() >= 4 && store.len() <= 6, "{}", store.len());
        assert!(store.evicted_segments.load(Ordering::Relaxed) >= 7);
        assert!(store.segments() <= 3);
        // A smaller capacity at the next start evicts down to it.
        drop(store);
        let store = SegmentStore::open_with(&dir.0, 2 * one, 2 * one).unwrap();
        assert!(store.disk_bytes() <= 2 * one);
        assert!(store.get("h/19").is_some());
    }

    #[test]
    fn oversized_entries_are_skipped() {
        let dir = TempDir::new("seg-big");
        let store = SegmentStore::open(&dir.0, 10).unwrap();
        assert!(
            !store
                .put(&"h/a".into(), &entry("", "too big for ten bytes"))
                .unwrap()
        );
        let long_key: Key = "k".repeat(70_000).into();
        let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
        assert!(!store.put(&long_key, &entry("", "x")).unwrap());
        // A record larger than a segment gets a segment of its own.
        let store = SegmentStore::open_with(&dir.0, 1 << 20, 64).unwrap();
        assert!(
            store
                .put(
                    &"h/a".into(),
                    &entry("", "a record larger than one segment")
                )
                .unwrap()
        );
        assert!(store.put(&"h/b".into(), &entry("", "and another")).unwrap());
        assert_eq!(
            store.get("h/a").unwrap().variants[0].body,
            "a record larger than one segment"
        );
        assert!(store.segments() >= 2);
    }

    #[test]
    fn a_torn_tail_is_truncated_and_never_served() {
        let dir = TempDir::new("seg-torn");
        {
            let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
            store.put(&"h/good".into(), &entry("", "good")).unwrap();
            store.put(&"h/torn".into(), &entry("", "torn")).unwrap();
        }
        let seg = only_segment(&dir);
        let full = fs::metadata(&seg).unwrap().len();
        let torn = frame_len("h/torn", "torn");
        // Cut the last frame in half, as SIGKILL mid-write would.
        let file = OpenOptions::new().write(true).open(&seg).unwrap();
        file.set_len(full - torn / 2).unwrap();
        drop(file);
        let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.get("h/good").is_some());
        assert!(store.get("h/torn").is_none());
        assert_eq!(store.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(fs::metadata(&seg).unwrap().len(), full - torn);
        // The next write lands after the good data.
        store.put(&"h/next".into(), &entry("", "next")).unwrap();
        drop(store);
        let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
        assert_eq!(store.len(), 2);
        assert!(store.get("h/next").is_some());
    }

    #[test]
    fn a_crash_at_any_byte_serves_only_whole_objects() {
        let dir = TempDir::new("seg-every-byte");
        let bodies = ["first", "second", "third"];
        {
            let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
            for (i, b) in bodies.iter().enumerate() {
                store.put(&format!("h/{i}").into(), &entry("", b)).unwrap();
            }
        }
        let seg = only_segment(&dir);
        let full = fs::read(&seg).unwrap();
        for cut in 0..=full.len() {
            fs::write(&seg, &full[..cut]).unwrap();
            let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
            for (i, b) in bodies.iter().enumerate() {
                if let Some(e) = store.get(&format!("h/{i}")) {
                    assert_eq!(e.variants[0].body, *b, "cut at {cut}");
                }
            }
            drop(store);
        }
        // Every whole frame survives a cut just after it.
        fs::write(&seg, &full).unwrap();
        assert_eq!(SegmentStore::open(&dir.0, 1 << 20).unwrap().len(), 3);
    }

    #[test]
    fn garbage_after_the_tail_and_flipped_bits_are_caught() {
        let dir = TempDir::new("seg-flip");
        {
            let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
            store.put(&"h/a".into(), &entry("", "first")).unwrap();
            store.put(&"h/b".into(), &entry("", "second")).unwrap();
        }
        let seg = only_segment(&dir);
        let mut bytes = fs::read(&seg).unwrap();
        // A flipped bit in the second frame's body: the scan stops there.
        let n = bytes.len();
        bytes[n - 3] ^= 0x40;
        bytes.extend_from_slice(b"EFS1 trailing junk");
        fs::write(&seg, &bytes).unwrap();
        let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.get("h/b").is_none());
        // A bit flipped after start is caught on read, and the object
        // dropped.
        let mut bytes = fs::read(&seg).unwrap();
        bytes[FRAME_HEADER + 20] ^= 0x01;
        fs::write(&seg, &bytes).unwrap();
        assert!(store.get("h/a").is_none());
        assert!(store.is_empty());
        assert_eq!(store.dropped.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_vanished_segment_is_a_miss() {
        let dir = TempDir::new("seg-vanish");
        let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
        store.put(&"h/a".into(), &entry("", "payload")).unwrap();
        fs::remove_file(only_segment(&dir)).unwrap();
        assert!(store.get("h/a").is_none());
        assert!(store.is_empty());
    }

    #[test]
    fn compaction_rewrites_a_mostly_dead_oldest_segment() {
        let dir = TempDir::new("seg-compact");
        let one = frame_len_tagged("h/0", "t", "x");
        let store = SegmentStore::open_with(&dir.0, 1 << 20, 4 * one).unwrap();
        for i in 0..4 {
            store
                .put(&format!("h/{i}").into(), &entry("t", "x"))
                .unwrap();
        }
        // Three of the first segment's four replaced: a quarter live.
        for i in 0..3 {
            store
                .put(&format!("h/{i}").into(), &entry("t", "y"))
                .unwrap();
        }
        assert_eq!(store.segments(), 2);
        let before = store.written_bytes.load(Ordering::Relaxed);
        assert!(store.compact().unwrap());
        assert_eq!(store.segments(), 1);
        assert_eq!(store.compacted_bytes.load(Ordering::Relaxed), one);
        assert_eq!(store.written_bytes.load(Ordering::Relaxed), before + one);
        assert!(store.write_amplification() > 1.0);
        for i in 0..4 {
            let want = if i < 3 { "y" } else { "x" };
            assert_eq!(store.get(&format!("h/{i}")).unwrap().variants[0].body, want);
        }
        // Nothing left worth compacting.
        assert!(!store.compact().unwrap());
        assert_eq!(store.purge_tag("t"), 4);
        drop(store);
        let store = SegmentStore::open_with(&dir.0, 1 << 20, 4 * one).unwrap();
        assert!(store.is_empty());
        assert_eq!(
            SegmentStore::open(&TempDir::new("seg-empty").0, 1 << 20)
                .unwrap()
                .write_amplification(),
            0.0
        );
    }

    #[test]
    fn a_snapshot_restart_replays_only_what_came_after() {
        let dir = TempDir::new("seg-snap");
        {
            let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
            store.put(&"h/a".into(), &entry("t1", "alpha")).unwrap();
            store.put(&"h/b".into(), &entry("t2", "beta")).unwrap();
            store.shutdown().unwrap();
        }
        assert!(dir.0.join(index::SNAPSHOT).exists());
        {
            let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
            assert!(store.warm);
            assert!(
                !dir.0.join(index::SNAPSHOT).exists(),
                "a loaded snapshot is removed"
            );
            assert_eq!(store.len(), 2);
            assert_eq!(store.purge_tag("t1"), 1);
            store.put(&"h/c".into(), &entry("", "gamma")).unwrap();
            // A crash now: no new snapshot.
        }
        let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
        assert!(!store.warm);
        assert_eq!(store.len(), 2);
        assert!(store.get("h/a").is_none());
        assert_eq!(store.get("h/c").unwrap().variants[0].body, "gamma");
        // Writes after the snapshot are replayed on top of it.
        store.shutdown().unwrap();
        drop(store);
        {
            let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
            store.put(&"h/d".into(), &entry("", "delta")).unwrap();
            store.remove("h/b");
            store.shutdown().unwrap();
        }
        let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
        assert!(store.warm);
        let mut keys: Vec<String> = ["h/b", "h/c", "h/d"]
            .iter()
            .filter(|k| store.get(k).is_some())
            .map(|k| k.to_string())
            .collect();
        keys.sort();
        assert_eq!(keys, ["h/c", "h/d"]);
    }

    #[test]
    fn a_snapshot_that_does_not_match_the_log_is_not_trusted() {
        let dir = TempDir::new("seg-snap-bad");
        {
            let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
            store.put(&"h/a".into(), &entry("", "alpha")).unwrap();
            store.put(&"h/b".into(), &entry("", "beta")).unwrap();
            store.shutdown().unwrap();
        }
        // The segment shrank behind the snapshot's back.
        let seg = only_segment(&dir);
        let len = fs::metadata(&seg).unwrap().len();
        OpenOptions::new()
            .write(true)
            .open(&seg)
            .unwrap()
            .set_len(len - 5)
            .unwrap();
        let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
        assert!(!store.warm);
        assert_eq!(store.len(), 1);
        assert!(store.get("h/b").is_none());
        drop(store);
        // A damaged snapshot falls back to the scan too.
        {
            let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
            store.shutdown().unwrap();
        }
        let snap = dir.0.join(index::SNAPSHOT);
        let mut bytes = fs::read(&snap).unwrap();
        bytes[10] ^= 0xff;
        fs::write(&snap, bytes).unwrap();
        let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
        assert!(!store.warm);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn files_of_the_old_layout_are_removed() {
        let dir = TempDir::new("seg-old");
        fs::create_dir_all(dir.0.join("tmp")).unwrap();
        fs::write(dir.0.join("0123.obj"), b"CACHET\0\x01 old").unwrap();
        fs::write(dir.0.join("tmp").join("half"), b"x").unwrap();
        fs::write(dir.0.join("notes.txt"), b"left alone").unwrap();
        fs::create_dir_all(dir.0.join("segments")).unwrap();
        fs::write(dir.0.join("segments").join("not-a-segment"), b"x").unwrap();
        let store = SegmentStore::open(&dir.0, 1 << 20).unwrap();
        assert!(store.is_empty());
        assert_eq!(store.dropped.load(Ordering::Relaxed), 1);
        assert!(!dir.0.join("0123.obj").exists() && !dir.0.join("tmp").exists());
        assert!(dir.0.join("notes.txt").exists());
    }

    #[test]
    fn open_fails_on_a_file_in_the_way() {
        let dir = TempDir::new("seg-blocked");
        fs::write(&dir.0, b"a file, not a directory").unwrap();
        assert!(SegmentStore::open(&dir.0, 1 << 20).is_err());
        fs::remove_file(&dir.0).unwrap();
    }

    #[test]
    fn segment_names_round_trip() {
        assert_eq!(segment_number(Path::new(&segment_name(42))), Some(42));
        for bad in ["x.seg", "000000000000002a.log", "zzzzzzzzzzzzzzzz.seg"] {
            assert_eq!(segment_number(Path::new(bad)), None, "{bad}");
        }
        assert!(parse_header(&[0u8; FRAME_HEADER]).is_none());
        let mut head = [0u8; FRAME_HEADER];
        head.copy_from_slice(&frame(PUT, b"")[..FRAME_HEADER]);
        head[4] = 9;
        assert!(parse_header(&head).is_none());
    }
}
