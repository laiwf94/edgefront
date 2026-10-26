//! The optional disk tier: one file per URL, a FIFO byte budget, and an
//! index rebuilt from the files at startup.
//!
//! Files are written to a temporary name and renamed into place, so a crash
//! leaves either the old record or the new one, never a torn one. Records
//! carry checksums; anything that fails them is deleted. All methods block,
//! so async callers run them on the blocking pool.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use sha2::{Digest, Sha256};

use super::codec;
use crate::object::{Entry, Key, surrogate_keys};

struct Meta {
    size: u64,
    tags: Vec<Arc<str>>,
    generation: u64,
}

#[derive(Default)]
struct Index {
    map: HashMap<Key, Meta>,
    fifo: VecDeque<(Key, u64)>,
    tags: HashMap<Arc<str>, HashSet<Key>>,
    bytes: u64,
    next_generation: u64,
}

pub struct DiskStore {
    dir: PathBuf,
    capacity: u64,
    index: Mutex<Index>,
    temp_counter: AtomicU64,
    /// Files found corrupt or foreign and deleted, at startup or on read.
    pub dropped: AtomicU64,
}

/// File name for a key: a hash, so keys never become paths.
fn file_name(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let mut name = String::with_capacity(36);
    for b in &digest[..16] {
        name.push_str(&format!("{b:02x}"));
    }
    name.push_str(".obj");
    name
}

impl DiskStore {
    /// Opens `dir`, creating it if needed, and rebuilds the index from the
    /// records in it, oldest first. Partial writes from a crash are removed.
    pub fn open(dir: &Path, capacity: u64) -> io::Result<Self> {
        fs::create_dir_all(dir.join("tmp"))?;
        for leftover in fs::read_dir(dir.join("tmp"))? {
            fs::remove_file(leftover?.path())?;
        }
        let store = DiskStore {
            dir: dir.to_path_buf(),
            capacity,
            index: Mutex::new(Index::default()),
            temp_counter: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        };
        let mut found = Vec::new();
        for item in fs::read_dir(dir)? {
            let item = item?;
            let path = item.path();
            if path.extension().is_some_and(|e| e == "obj") && item.file_type()?.is_file() {
                found.push((item.metadata()?.modified()?, path));
            }
        }
        found.sort();
        for (_, path) in found {
            match read_header(&path) {
                Ok((key, tags, size))
                    if path.file_name().is_some_and(|n| *n == *file_name(&key)) =>
                {
                    let mut index = store.lock();
                    store.index_insert(&mut index, key, size, tags);
                    store.evict(&mut index);
                }
                _ => store.drop_file(&path),
            }
        }
        Ok(store)
    }

    fn lock(&self) -> MutexGuard<'_, Index> {
        self.index.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn drop_file(&self, path: &Path) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        let _ = fs::remove_file(path);
    }

    fn index_insert(&self, index: &mut Index, key: Key, size: u64, tags: Vec<Arc<str>>) {
        if let Some(old) = index.map.remove(&key) {
            unindex(index, &key, &old);
        }
        let generation = index.next_generation;
        index.next_generation += 1;
        for tag in &tags {
            index
                .tags
                .entry(tag.clone())
                .or_default()
                .insert(key.clone());
        }
        index.bytes += size;
        index.fifo.push_back((key.clone(), generation));
        index.map.insert(
            key,
            Meta {
                size,
                tags,
                generation,
            },
        );
    }

    /// Deletes the oldest records until the budget holds.
    fn evict(&self, index: &mut Index) {
        while index.bytes > self.capacity {
            let Some((key, generation)) = index.fifo.pop_front() else {
                break;
            };
            if index
                .map
                .get(&key)
                .is_some_and(|m| m.generation == generation)
            {
                let meta = index.map.remove(&key).expect("checked above");
                unindex(index, &key, &meta);
                let _ = fs::remove_file(self.dir.join(file_name(&key)));
            }
        }
        if index.fifo.len() > 2 * index.map.len() + 64 {
            let map = &index.map;
            let live: VecDeque<_> = index
                .fifo
                .iter()
                .filter(|(k, g)| map.get(k).is_some_and(|m| m.generation == *g))
                .cloned()
                .collect();
            index.fifo = live;
        }
    }

    /// Writes an entry. Returns false if it was too large to keep.
    pub fn put(&self, key: &Key, entry: &Entry) -> io::Result<bool> {
        let Some(record) = codec::encode(key, entry) else {
            return Ok(false);
        };
        let size = record.len() as u64;
        if size > self.capacity {
            return Ok(false);
        }
        let n = self.temp_counter.fetch_add(1, Ordering::Relaxed);
        let temp = self.dir.join("tmp").join(format!("{}.{n}", file_name(key)));
        let mut file = File::create(&temp)?;
        file.write_all(&record)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, self.dir.join(file_name(key)))?;
        let mut index = self.lock();
        self.index_insert(&mut index, key.clone(), size, entry.tags());
        self.evict(&mut index);
        Ok(true)
    }

    /// Reads an entry. A record that fails its checksums is deleted.
    pub fn get(&self, key: &str) -> Option<Entry> {
        if !self.lock().map.contains_key(key) {
            return None;
        }
        let path = self.dir.join(file_name(key));
        match fs::read(&path).ok().map(|b| codec::decode(&b)) {
            Some(Ok((stored_key, entry))) if &*stored_key == key => Some(entry),
            Some(_) => {
                self.remove(key);
                self.dropped.fetch_add(1, Ordering::Relaxed);
                None
            }
            None => {
                self.remove(key);
                None
            }
        }
    }

    /// Removes an entry and its file.
    pub fn remove(&self, key: &str) -> bool {
        let mut index = self.lock();
        let Some((key, meta)) = index.map.remove_entry(key) else {
            return false;
        };
        unindex(&mut index, &key, &meta);
        let _ = fs::remove_file(self.dir.join(file_name(&key)));
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

    pub fn bytes(&self) -> u64 {
        self.lock().bytes
    }

    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn unindex(index: &mut Index, key: &Key, meta: &Meta) {
    index.bytes -= meta.size;
    for tag in &meta.tags {
        if let Some(keys) = index.tags.get_mut(tag) {
            keys.remove(key);
            if keys.is_empty() {
                index.tags.remove(tag);
            }
        }
    }
}

/// Reads and verifies only the header section of a record.
fn read_header(path: &Path) -> Result<(Key, Vec<Arc<str>>, u64), codec::DecodeError> {
    let bad = |_| codec::DecodeError::Truncated;
    let mut file = File::open(path).map_err(bad)?;
    let size = file.metadata().map_err(bad)?.len();
    let mut prefix = [0u8; codec::PREFIX];
    file.read_exact(&mut prefix).map_err(bad)?;
    let span = codec::header_span(&prefix)?;
    let mut buf = prefix.to_vec();
    buf.resize(span, 0);
    file.read_exact(&mut buf[codec::PREFIX..]).map_err(bad)?;
    let header = codec::decode_header(&buf)?;
    let mut tags: Vec<Arc<str>> = Vec::new();
    for v in &header.variants {
        for t in surrogate_keys(&v.headers) {
            if !tags.contains(&t) {
                tags.push(t);
            }
        }
    }
    Ok((header.key, tags, size))
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

    fn entry(tags: &str, body: &'static str) -> Entry {
        Entry::single(object(&[("surrogate-key", tags)], &[], body))
    }

    #[test]
    fn put_get_and_reopen() {
        let dir = TempDir::new("reopen");
        {
            let store = DiskStore::open(&dir.0, 1 << 20).unwrap();
            assert!(store.is_empty());
            assert!(store.put(&"h/a".into(), &entry("t1", "alpha")).unwrap());
            assert!(store.put(&"h/b".into(), &entry("t1 t2", "beta")).unwrap());
            assert_eq!(store.get("h/a").unwrap().variants[0].body, "alpha");
            assert!(store.get("h/missing").is_none());
        }
        let store = DiskStore::open(&dir.0, 1 << 20).unwrap();
        assert_eq!(store.len(), 2);
        assert_eq!(store.get("h/b").unwrap().variants[0].body, "beta");
        assert_eq!(store.purge_tag("t1"), 2);
        assert!(store.is_empty());
        assert_eq!(store.bytes(), 0);
        assert_eq!(
            fs::read_dir(&dir.0).unwrap().count(),
            1,
            "only tmp/ should remain"
        );
    }

    #[test]
    fn replacing_a_key_keeps_one_file() {
        let dir = TempDir::new("replace");
        let store = DiskStore::open(&dir.0, 1 << 20).unwrap();
        store.put(&"h/a".into(), &entry("t1", "one")).unwrap();
        store.put(&"h/a".into(), &entry("t2", "two")).unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(store.get("h/a").unwrap().variants[0].body, "two");
        assert_eq!(store.purge_tag("t1"), 0);
        assert_eq!(store.purge_tag("t2"), 1);
    }

    #[test]
    fn fifo_budget() {
        let dir = TempDir::new("fifo");
        let one = codec::encode("h/0", &entry("", "x")).unwrap().len() as u64;
        let store = DiskStore::open(&dir.0, 3 * one).unwrap();
        for i in 0..10 {
            store
                .put(&format!("h/{i}").into(), &entry("", "x"))
                .unwrap();
            assert!(store.bytes() <= 3 * one);
        }
        assert_eq!(store.len(), 3);
        assert!(store.get("h/0").is_none());
        assert!(store.get("h/9").is_some());
        // The same budget applies when reopening with a smaller one.
        drop(store);
        let store = DiskStore::open(&dir.0, one).unwrap();
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn oversized_entries_are_skipped() {
        let dir = TempDir::new("big");
        let store = DiskStore::open(&dir.0, 10).unwrap();
        assert!(
            !store
                .put(&"h/a".into(), &entry("", "too big for ten bytes"))
                .unwrap()
        );
        let long_key: Key = "k".repeat(70_000).into();
        let store = DiskStore::open(&dir.0, 1 << 20).unwrap();
        assert!(!store.put(&long_key, &entry("", "x")).unwrap());
    }

    #[test]
    fn corrupt_and_foreign_files_are_dropped_at_startup() {
        let dir = TempDir::new("corrupt");
        {
            let store = DiskStore::open(&dir.0, 1 << 20).unwrap();
            store.put(&"h/good".into(), &entry("", "good")).unwrap();
            store.put(&"h/flip".into(), &entry("", "flip")).unwrap();
            store.put(&"h/cut".into(), &entry("", "cut")).unwrap();
        }
        let flip = dir.0.join(file_name("h/flip"));
        let mut bytes = fs::read(&flip).unwrap();
        bytes[20] ^= 0xff;
        fs::write(&flip, bytes).unwrap();
        let cut = dir.0.join(file_name("h/cut"));
        let bytes = fs::read(&cut).unwrap();
        fs::write(&cut, &bytes[..10]).unwrap();
        fs::write(dir.0.join("deadbeef.obj"), b"not a record at all").unwrap();
        // A valid record stored under the wrong file name.
        fs::copy(dir.0.join(file_name("h/good")), dir.0.join("0000.obj")).unwrap();
        fs::write(dir.0.join("tmp").join("half-written"), b"CACHET").unwrap();
        fs::write(dir.0.join("notes.txt"), b"left alone").unwrap();

        let store = DiskStore::open(&dir.0, 1 << 20).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.get("h/good").is_some());
        assert_eq!(store.dropped.load(Ordering::Relaxed), 4);
        assert!(dir.0.join("notes.txt").exists());
        assert_eq!(fs::read_dir(dir.0.join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn body_corruption_is_caught_on_read() {
        let dir = TempDir::new("body");
        let store = DiskStore::open(&dir.0, 1 << 20).unwrap();
        store.put(&"h/a".into(), &entry("", "payload")).unwrap();
        let path = dir.0.join(file_name("h/a"));
        let mut bytes = fs::read(&path).unwrap();
        let n = bytes.len();
        bytes[n - 40] ^= 0xff;
        fs::write(&path, bytes).unwrap();
        assert!(store.get("h/a").is_none());
        assert_eq!(store.dropped.load(Ordering::Relaxed), 1);
        assert!(store.is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn a_vanished_file_is_a_miss() {
        let dir = TempDir::new("vanish");
        let store = DiskStore::open(&dir.0, 1 << 20).unwrap();
        store.put(&"h/a".into(), &entry("", "payload")).unwrap();
        fs::remove_file(dir.0.join(file_name("h/a"))).unwrap();
        assert!(store.get("h/a").is_none());
        assert!(store.is_empty());
        assert!(!store.remove("h/a"));
    }

    #[test]
    fn many_replacements_keep_the_queue_bounded() {
        let dir = TempDir::new("queue");
        let store = DiskStore::open(&dir.0, 1 << 20).unwrap();
        for i in 0..300 {
            store
                .put(&format!("h/{}", i % 3).into(), &entry("", "x"))
                .unwrap();
        }
        assert!(store.lock().fifo.len() <= 2 * 3 + 65);
    }

    #[test]
    fn open_fails_on_a_file_in_the_way() {
        let dir = TempDir::new("blocked");
        fs::write(&dir.0, b"a file, not a directory").unwrap();
        assert!(DiskStore::open(&dir.0, 1 << 20).is_err());
        fs::remove_file(&dir.0).unwrap();
    }
}
