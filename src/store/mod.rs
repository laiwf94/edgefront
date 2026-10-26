//! Cache storage: the memory tier, purges and the rules tying them together.

pub mod memory;

use std::sync::Arc;

use http::HeaderMap;

use crate::object::{CachedObject, Entry, Key};
use crate::purge::{PurgeLog, PurgeTarget};
use memory::MemoryStore;

/// Sizing for a [`Cache`].
#[derive(Debug, Clone)]
pub struct CacheSettings {
    pub memory_bytes: usize,
    pub shards: usize,
    pub max_object_bytes: usize,
    pub max_variants: usize,
    pub purge_log: usize,
}

impl Default for CacheSettings {
    fn default() -> Self {
        CacheSettings {
            memory_bytes: 256 << 20,
            shards: 16,
            max_object_bytes: 8 << 20,
            max_variants: 8,
            purge_log: 4096,
        }
    }
}

pub struct Cache {
    memory: MemoryStore,
    log: PurgeLog,
    max_variants: usize,
}

impl Cache {
    pub fn new(settings: &CacheSettings) -> Self {
        Cache {
            memory: MemoryStore::new(
                settings.memory_bytes,
                settings.shards,
                settings.max_object_bytes,
            ),
            log: PurgeLog::new(settings.purge_log),
            max_variants: settings.max_variants,
        }
    }

    pub fn memory(&self) -> &MemoryStore {
        &self.memory
    }

    /// The stored variant matching this request, if any.
    pub fn lookup(&self, key: &str, req: &HeaderMap) -> Option<Arc<CachedObject>> {
        self.memory.get(key)?.find(req).cloned()
    }

    /// The purge epoch; a fetch takes it before going to the origin.
    pub fn epoch(&self) -> u64 {
        self.log.epoch()
    }

    /// Stores a response fetched since `epoch`. Returns false when it was not
    /// kept: too large, or purged while the fetch was in flight.
    ///
    /// The object is inserted first and the log checked second. A purge
    /// records itself before removing, so either the purge removes this
    /// insert or this check sees the purge.
    pub fn store(&self, key: Key, obj: Arc<CachedObject>, req: &HeaderMap, epoch: u64) -> bool {
        let tags = obj.tags.clone();
        let entry = match self.memory.peek(&key) {
            Some(existing) => existing.with_variant(obj, req, self.max_variants),
            None => Entry::single(obj),
        };
        if !self.memory.insert(key.clone(), Arc::new(entry)).stored {
            return false;
        }
        if self.log.purged_since(epoch, &key, &tags) {
            self.memory.remove(&key);
            return false;
        }
        true
    }

    /// Removes everything the targets name and returns how many URLs went.
    pub fn purge(&self, targets: &[PurgeTarget]) -> usize {
        self.log.record(targets);
        let mut removed = 0;
        for target in targets {
            removed += match target {
                PurgeTarget::Tag(tag) => self.memory.purge_tag(tag).len(),
                PurgeTarget::Key(key) => usize::from(self.memory.remove(key).is_some()),
            };
        }
        removed
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
    }

    #[test]
    fn store_and_lookup() {
        let c = cache();
        let req = HeaderMap::new();
        assert!(c.store("h/a".into(), obj("t1", "a"), &req, c.epoch()));
        assert_eq!(c.lookup("h/a", &req).unwrap().body, "a");
        assert!(c.lookup("h/b", &req).is_none());
        assert_eq!(c.memory().len(), 1);
    }

    #[test]
    fn variants_accumulate_under_one_key() {
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
            assert!(c.store("h/a".into(), o, &req, c.epoch()));
        }
        assert_eq!(
            c.lookup("h/a", &map(&[("accept-language", "fr")]))
                .unwrap()
                .body,
            "fr"
        );
        assert_eq!(
            c.lookup("h/a", &map(&[("accept-language", "en")]))
                .unwrap()
                .body,
            "en"
        );
    }

    #[test]
    fn purge_by_tag_and_key() {
        let c = cache();
        let req = HeaderMap::new();
        c.store("h/a".into(), obj("product home", "a"), &req, c.epoch());
        c.store("h/b".into(), obj("product", "b"), &req, c.epoch());
        c.store("h/c".into(), obj("news", "c"), &req, c.epoch());
        assert_eq!(c.memory().tag_count(), 3);
        assert_eq!(c.purge(&[PurgeTarget::Tag("product".into())]), 2);
        assert!(c.lookup("h/a", &req).is_none());
        assert!(c.lookup("h/b", &req).is_none());
        assert!(c.lookup("h/c", &req).is_some());
        // "home" pointed only at h/a, which is gone, so the index forgot it.
        assert_eq!(c.memory().tag_count(), 1);
        assert_eq!(c.purge(&[PurgeTarget::Key("h/c".into())]), 1);
        assert_eq!(c.memory().tag_count(), 0);
        assert_eq!(
            c.purge(&[
                PurgeTarget::Key("h/c".into()),
                PurgeTarget::Tag("none".into())
            ]),
            0
        );
    }

    #[test]
    fn purge_during_fetch_keeps_old_response_out() {
        let c = cache();
        let req = HeaderMap::new();
        let started = c.epoch();
        c.purge(&[PurgeTarget::Tag("product".into())]);
        assert!(!c.store("h/a".into(), obj("product", "old"), &req, started));
        assert!(c.lookup("h/a", &req).is_none());
        // An unrelated purge does not block the store.
        let started = c.epoch();
        c.purge(&[PurgeTarget::Tag("other".into())]);
        assert!(c.store("h/a".into(), obj("product", "new"), &req, started));
    }

    #[test]
    fn oversized_store_is_refused() {
        let c = Cache::new(&CacheSettings {
            memory_bytes: 1 << 20,
            shards: 1,
            max_object_bytes: 300,
            ..CacheSettings::default()
        });
        let big = Arc::new(CachedObject::new(
            StatusCode::OK,
            HeaderMap::new(),
            Bytes::from(vec![0; 1_000]),
            obj("", "").freshness,
            &HeaderMap::new(),
        ));
        assert!(!c.store("h/a".into(), big, &HeaderMap::new(), c.epoch()));
    }

    #[test]
    fn eviction_keeps_tag_index_in_step() {
        let c = Cache::new(&CacheSettings {
            memory_bytes: 4_000,
            shards: 1,
            max_object_bytes: 4_000,
            ..CacheSettings::default()
        });
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
        // One tag per live object plus the shared "all".
        assert_eq!(c.memory().tag_count(), live + 1);
        assert_eq!(c.purge(&[PurgeTarget::Tag("all".into())]), live);
        assert_eq!(c.memory().tag_count(), 0);
    }
}
