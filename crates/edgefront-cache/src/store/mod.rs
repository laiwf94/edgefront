//! Cache storage: the memory tier, the optional disk tier, purges, and the
//! rules tying them together.
//!
//! The disk tier is write-through: every stored entry is queued for disk,
//! and memory holds the hot subset. A restart therefore comes back warm, and
//! a memory eviction costs nothing. Disk writes happen on one background
//! thread fed by a bounded queue; when the queue is full the write is
//! skipped, because the entry is still in memory and a cache may forget.

pub mod codec;
pub mod disk;
pub mod memory;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};

use http::HeaderMap;
use tokio::sync::oneshot;

use crate::object::{CachedObject, Entry, Key};
use crate::purge::{PurgeLog, PurgeTarget};
use disk::DiskStore;
use memory::MemoryStore;

/// Sizing for a [`Cache`].
#[derive(Debug, Clone)]
pub struct CacheSettings {
    pub memory_bytes: usize,
    pub shards: usize,
    pub max_object_bytes: usize,
    pub max_variants: usize,
    pub purge_log: usize,
    pub disk: Option<DiskSettings>,
}

#[derive(Debug, Clone)]
pub struct DiskSettings {
    pub path: PathBuf,
    pub max_bytes: u64,
    pub queue: usize,
}

impl Default for CacheSettings {
    fn default() -> Self {
        CacheSettings {
            memory_bytes: 256 << 20,
            shards: 16,
            max_object_bytes: 8 << 20,
            max_variants: 8,
            purge_log: 4096,
            disk: None,
        }
    }
}

/// Counters the metrics endpoint reads.
#[derive(Debug, Default)]
pub struct CacheStats {
    pub memory_evictions: AtomicU64,
    pub disk_hits: AtomicU64,
    pub disk_writes: AtomicU64,
    pub disk_write_errors: AtomicU64,
    pub disk_queue_full: AtomicU64,
}

enum Job {
    Write {
        key: Key,
        entry: Arc<Entry>,
        epoch: u64,
    },
    Flush(oneshot::Sender<()>),
}

pub struct Cache {
    memory: MemoryStore,
    disk: Option<Arc<DiskStore>>,
    writer: Option<SyncSender<Job>>,
    log: Arc<PurgeLog>,
    max_variants: usize,
    pub stats: Arc<CacheStats>,
}

impl Cache {
    /// Builds the cache, opening and indexing the disk tier if configured.
    pub fn new(settings: &CacheSettings) -> io::Result<Self> {
        let log = Arc::new(PurgeLog::new(settings.purge_log));
        let stats = Arc::new(CacheStats::default());
        let (disk, writer) = match &settings.disk {
            None => (None, None),
            Some(d) => {
                let disk = Arc::new(DiskStore::open(&d.path, d.max_bytes)?);
                let (tx, rx) = sync_channel(d.queue.max(1));
                let (disk2, log2, stats2) = (disk.clone(), log.clone(), stats.clone());
                std::thread::Builder::new()
                    .name("cachet-disk".into())
                    .spawn(move || write_loop(rx, &disk2, &log2, &stats2))?;
                (Some(disk), Some(tx))
            }
        };
        Ok(Cache {
            memory: MemoryStore::new(
                settings.memory_bytes,
                settings.shards,
                settings.max_object_bytes,
            ),
            disk,
            writer,
            log,
            max_variants: settings.max_variants,
            stats,
        })
    }

    pub fn memory(&self) -> &MemoryStore {
        &self.memory
    }

    pub fn disk(&self) -> Option<&DiskStore> {
        self.disk.as_deref()
    }

    /// The stored variant matching this request, from memory or disk. A disk
    /// hit is copied back into memory.
    pub async fn lookup(&self, key: &Key, req: &HeaderMap) -> Option<Arc<CachedObject>> {
        if let Some(entry) = self.memory.get(key) {
            return entry.find(req).cloned();
        }
        let disk = self.disk.clone()?;
        let epoch = self.epoch();
        let k = key.clone();
        let entry = Arc::new(
            tokio::task::spawn_blocking(move || disk.get(&k))
                .await
                .ok()??,
        );
        let found = entry.find(req).cloned();
        let out = self.memory.insert(key.clone(), entry.clone());
        self.count_evictions(out.evicted.len());
        if out.stored && self.log.purged_since(epoch, key, &entry.tags()) {
            self.memory.remove(key);
            return None;
        }
        self.stats.disk_hits.fetch_add(1, Ordering::Relaxed);
        found
    }

    /// The purge epoch; a fetch takes it before going to the origin.
    pub fn epoch(&self) -> u64 {
        self.log.epoch()
    }

    fn count_evictions(&self, n: usize) {
        self.stats
            .memory_evictions
            .fetch_add(n as u64, Ordering::Relaxed);
    }

    /// Stores a response fetched since `epoch`. Returns false when it was not
    /// kept: too large, or purged while the fetch was in flight.
    ///
    /// The object is inserted first and the log checked second. A purge
    /// records itself before removing, so either the purge removes this
    /// insert or this check sees the purge.
    pub fn store(&self, key: Key, obj: Arc<CachedObject>, req: &HeaderMap, epoch: u64) -> bool {
        let tags = obj.tags.clone();
        let entry = Arc::new(match self.memory.peek(&key) {
            Some(existing) => existing.with_variant(obj, req, self.max_variants),
            None => Entry::single(obj),
        });
        let out = self.memory.insert(key.clone(), entry.clone());
        self.count_evictions(out.evicted.len());
        if !out.stored {
            return false;
        }
        if self.log.purged_since(epoch, &key, &tags) {
            self.memory.remove(&key);
            return false;
        }
        if let Some(writer) = &self.writer {
            match writer.try_send(Job::Write { key, entry, epoch }) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    self.stats.disk_queue_full.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.stats.disk_write_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        true
    }

    /// Removes everything the targets name from both tiers and returns how
    /// many URLs went.
    pub async fn purge(&self, targets: &[PurgeTarget]) -> usize {
        self.log.record(targets);
        let mut removed = 0;
        for target in targets {
            removed += match target {
                PurgeTarget::Tag(tag) => self.memory.purge_tag(tag).len(),
                PurgeTarget::Key(key) => usize::from(self.memory.remove(key).is_some()),
            };
        }
        if let Some(disk) = self.disk.clone() {
            let targets = targets.to_vec();
            let on_disk = tokio::task::spawn_blocking(move || {
                targets
                    .iter()
                    .map(|t| match t {
                        PurgeTarget::Tag(tag) => disk.purge_tag(tag),
                        PurgeTarget::Key(key) => usize::from(disk.remove(key)),
                    })
                    .sum::<usize>()
            })
            .await
            .unwrap_or(0);
            removed = removed.max(on_disk);
        }
        removed
    }

    /// Waits until every disk write queued so far has finished.
    pub async fn flush(&self) {
        let Some(writer) = self.writer.clone() else {
            return;
        };
        let (tx, rx) = oneshot::channel();
        let sent = tokio::task::spawn_blocking(move || writer.send(Job::Flush(tx)).is_ok()).await;
        if sent.unwrap_or(false) {
            let _ = rx.await;
        }
    }
}

fn write_loop(
    rx: std::sync::mpsc::Receiver<Job>,
    disk: &DiskStore,
    log: &PurgeLog,
    stats: &CacheStats,
) {
    for job in rx {
        match job {
            Job::Write { key, entry, epoch } => {
                let tags = entry.tags();
                if log.purged_since(epoch, &key, &tags) {
                    continue;
                }
                match disk.put(&key, &entry) {
                    Ok(true) => {
                        stats.disk_writes.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(false) => {}
                    Err(_) => {
                        stats.disk_write_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
                // Same insert-then-check rule as the memory tier.
                if log.purged_since(epoch, &key, &tags) {
                    disk.remove(&key);
                }
            }
            Job::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use bytes::Bytes;
    use http::StatusCode;

    use super::*;
    use crate::object::tests::map;
    use crate::policy::Freshness;
    use disk::tests::TempDir;

    fn obj(tags: &str, body: &'static str) -> Arc<CachedObject> {
        let f = Freshness {
            lifetime: Duration::from_secs(60),
            corrected_initial_age: Duration::ZERO,
            response_time: SystemTime::UNIX_EPOCH,
        };
        let headers = map(&[("surrogate-key", tags)]);
        Arc::new(CachedObject::new(
            StatusCode::OK,
            headers,
            Bytes::from_static(body.as_bytes()),
            f,
            &HeaderMap::new(),
        ))
    }

    fn cache() -> Cache {
        Cache::new(&CacheSettings {
            memory_bytes: 1 << 20,
            shards: 4,
            ..CacheSettings::default()
        })
        .unwrap()
    }

    fn with_disk(dir: &TempDir, memory_bytes: usize) -> Cache {
        Cache::new(&CacheSettings {
            memory_bytes,
            shards: 1,
            max_object_bytes: memory_bytes,
            disk: Some(DiskSettings {
                path: dir.0.clone(),
                max_bytes: 1 << 20,
                queue: 64,
            }),
            ..CacheSettings::default()
        })
        .unwrap()
    }

    fn k(s: &str) -> Key {
        s.into()
    }

    #[tokio::test]
    async fn store_and_lookup() {
        let c = cache();
        let req = HeaderMap::new();
        assert!(c.store(k("h/a"), obj("t1", "a"), &req, c.epoch()));
        assert_eq!(c.lookup(&k("h/a"), &req).await.unwrap().body, "a");
        assert!(c.lookup(&k("h/b"), &req).await.is_none());
        assert_eq!(c.memory().len(), 1);
        assert!(c.disk().is_none());
        c.flush().await;
    }

    #[tokio::test]
    async fn variants_accumulate_under_one_key() {
        let c = cache();
        let headers = map(&[("vary", "accept-language")]);
        let f = obj("", "").freshness;
        for lang in ["en", "fr"] {
            let req = map(&[("accept-language", lang)]);
            let body = Bytes::from(lang.to_string());
            let o = Arc::new(CachedObject::new(
                StatusCode::OK,
                headers.clone(),
                body,
                f,
                &req,
            ));
            assert!(c.store(k("h/a"), o, &req, c.epoch()));
        }
        let fr = c
            .lookup(&k("h/a"), &map(&[("accept-language", "fr")]))
            .await;
        assert_eq!(fr.unwrap().body, "fr");
        let en = c
            .lookup(&k("h/a"), &map(&[("accept-language", "en")]))
            .await;
        assert_eq!(en.unwrap().body, "en");
        assert!(
            c.lookup(&k("h/a"), &map(&[("accept-language", "de")]))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn purge_by_tag_and_key() {
        let c = cache();
        let req = HeaderMap::new();
        c.store(k("h/a"), obj("product home", "a"), &req, c.epoch());
        c.store(k("h/b"), obj("product", "b"), &req, c.epoch());
        c.store(k("h/c"), obj("news", "c"), &req, c.epoch());
        assert_eq!(c.memory().tag_count(), 3);
        assert_eq!(c.purge(&[PurgeTarget::Tag("product".into())]).await, 2);
        assert!(c.lookup(&k("h/a"), &req).await.is_none());
        assert!(c.lookup(&k("h/b"), &req).await.is_none());
        assert!(c.lookup(&k("h/c"), &req).await.is_some());
        // "home" pointed only at h/a, which is gone, so the index forgot it.
        assert_eq!(c.memory().tag_count(), 1);
        assert_eq!(c.purge(&[PurgeTarget::Key(k("h/c"))]).await, 1);
        assert_eq!(c.memory().tag_count(), 0);
        let none = [PurgeTarget::Key(k("h/c")), PurgeTarget::Tag("none".into())];
        assert_eq!(c.purge(&none).await, 0);
    }

    #[tokio::test]
    async fn purge_during_fetch_keeps_old_response_out() {
        let c = cache();
        let req = HeaderMap::new();
        let started = c.epoch();
        c.purge(&[PurgeTarget::Tag("product".into())]).await;
        assert!(!c.store(k("h/a"), obj("product", "old"), &req, started));
        assert!(c.lookup(&k("h/a"), &req).await.is_none());
        // An unrelated purge does not block the store.
        let started = c.epoch();
        c.purge(&[PurgeTarget::Tag("other".into())]).await;
        assert!(c.store(k("h/a"), obj("product", "new"), &req, started));
    }

    #[tokio::test]
    async fn oversized_store_is_refused() {
        let c = Cache::new(&CacheSettings {
            memory_bytes: 1 << 20,
            shards: 1,
            max_object_bytes: 300,
            ..CacheSettings::default()
        })
        .unwrap();
        let big = Arc::new(CachedObject::new(
            StatusCode::OK,
            HeaderMap::new(),
            Bytes::from(vec![0; 1_000]),
            obj("", "").freshness,
            &HeaderMap::new(),
        ));
        assert!(!c.store(k("h/a"), big, &HeaderMap::new(), c.epoch()));
    }

    #[tokio::test]
    async fn eviction_keeps_tag_index_in_step() {
        let c = Cache::new(&CacheSettings {
            memory_bytes: 4_000,
            shards: 1,
            max_object_bytes: 4_000,
            ..CacheSettings::default()
        })
        .unwrap();
        for i in 0..50 {
            c.store(
                format!("h/{i}").into(),
                obj(&format!("t{i} all"), "x"),
                &HeaderMap::new(),
                c.epoch(),
            );
        }
        let live = c.memory().len();
        assert!(live < 50);
        assert!(c.stats.memory_evictions.load(Ordering::Relaxed) > 0);
        // One tag per live object plus the shared "all".
        assert_eq!(c.memory().tag_count(), live + 1);
        assert_eq!(c.purge(&[PurgeTarget::Tag("all".into())]).await, live);
        assert_eq!(c.memory().tag_count(), 0);
    }

    #[tokio::test]
    async fn disk_keeps_what_memory_evicts() {
        let dir = TempDir::new("tier-evict");
        let c = with_disk(&dir, 3_000);
        let req = HeaderMap::new();
        for i in 0..30 {
            assert!(c.store(format!("h/{i}").into(), obj("all", "x"), &req, c.epoch()));
        }
        c.flush().await;
        assert!(c.memory().len() < 30);
        assert_eq!(c.disk().unwrap().len(), 30);
        // h/0 was evicted from memory long ago; the disk tier brings it back.
        assert!(c.memory().peek("h/0").is_none());
        assert_eq!(c.lookup(&k("h/0"), &req).await.unwrap().body, "x");
        assert!(c.memory().peek("h/0").is_some());
        assert_eq!(c.stats.disk_hits.load(Ordering::Relaxed), 1);
        assert_eq!(c.stats.disk_writes.load(Ordering::Relaxed), 30);
        assert_eq!(c.purge(&[PurgeTarget::Tag("all".into())]).await, 30);
        assert!(c.disk().unwrap().is_empty());
    }

    #[tokio::test]
    async fn restart_comes_back_warm() {
        let dir = TempDir::new("tier-restart");
        {
            let c = with_disk(&dir, 1 << 20);
            c.store(k("h/a"), obj("", "kept"), &HeaderMap::new(), c.epoch());
            c.flush().await;
        }
        let c = with_disk(&dir, 1 << 20);
        assert_eq!(
            c.lookup(&k("h/a"), &HeaderMap::new()).await.unwrap().body,
            "kept"
        );
    }

    #[tokio::test]
    async fn queued_write_purged_before_it_lands_is_skipped() {
        let dir = TempDir::new("tier-race");
        let c = with_disk(&dir, 1 << 20);
        let started = c.epoch();
        assert!(c.store(k("h/a"), obj("t", "old"), &HeaderMap::new(), started));
        // Purge the memory copy only, as if it raced with the queued write.
        c.log.record(&[PurgeTarget::Tag("t".into())]);
        c.memory().remove("h/a");
        c.flush().await;
        assert!(c.disk().unwrap().is_empty());
        assert!(c.lookup(&k("h/a"), &HeaderMap::new()).await.is_none());
    }

    #[tokio::test]
    async fn full_queue_skips_the_disk_write() {
        let dir = TempDir::new("tier-full");
        let mut settings = CacheSettings {
            memory_bytes: 1 << 20,
            shards: 1,
            ..CacheSettings::default()
        };
        settings.disk = Some(DiskSettings {
            path: dir.0.clone(),
            max_bytes: 1 << 20,
            queue: 1,
        });
        let c = Cache::new(&settings).unwrap();
        for i in 0..200 {
            c.store(
                format!("h/{i}").into(),
                obj("", "x"),
                &HeaderMap::new(),
                c.epoch(),
            );
        }
        c.flush().await;
        let written = c.stats.disk_writes.load(Ordering::Relaxed);
        let skipped = c.stats.disk_queue_full.load(Ordering::Relaxed);
        assert_eq!(written + skipped, 200);
        assert_eq!(c.memory().len(), 200);
    }

    #[test]
    fn unusable_disk_path_fails_at_startup() {
        let dir = TempDir::new("tier-bad");
        std::fs::write(&dir.0, b"file").unwrap();
        let settings = CacheSettings {
            disk: Some(DiskSettings {
                path: dir.0.clone(),
                max_bytes: 1,
                queue: 1,
            }),
            ..CacheSettings::default()
        };
        assert!(Cache::new(&settings).is_err());
        std::fs::remove_file(&dir.0).unwrap();
    }
}
