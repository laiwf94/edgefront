//! Purge targets and the purge log.
//!
//! A fetch that started before a purge can finish after it. Without care,
//! it would put the old response back into the cache. Each fetch notes the
//! log's epoch when it starts; after storing, the cache asks the log whether
//! a matching purge happened since, and removes the object if so.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use crate::object::Key;

/// What a purge removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PurgeTarget {
    /// Every object carrying this surrogate key.
    Tag(Arc<str>),
    /// Every variant stored under this primary key.
    Key(Key),
}

impl PurgeTarget {
    fn matches(&self, key: &str, tags: &[Arc<str>]) -> bool {
        match self {
            PurgeTarget::Tag(t) => tags.iter().any(|x| x == t),
            PurgeTarget::Key(k) => &**k == key,
        }
    }
}

struct Inner {
    epoch: u64,
    events: VecDeque<(u64, PurgeTarget)>,
}

/// A bounded record of recent purges.
pub struct PurgeLog {
    inner: Mutex<Inner>,
    capacity: usize,
}

impl PurgeLog {
    pub fn new(capacity: usize) -> Self {
        PurgeLog {
            inner: Mutex::new(Inner {
                epoch: 0,
                events: VecDeque::new(),
            }),
            capacity: capacity.max(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The current epoch. A fetch records this before it starts.
    pub fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// Records a purge and returns its epoch.
    pub fn record(&self, targets: &[PurgeTarget]) -> u64 {
        let mut inner = self.lock();
        inner.epoch += 1;
        let epoch = inner.epoch;
        for t in targets {
            inner.events.push_back((epoch, t.clone()));
        }
        while inner.events.len() > self.capacity {
            inner.events.pop_front();
        }
        epoch
    }

    /// Whether a purge recorded after `epoch` matches this object. When the
    /// log no longer reaches back to `epoch`, the answer is a conservative yes.
    pub fn purged_since(&self, epoch: u64, key: &str, tags: &[Arc<str>]) -> bool {
        let inner = self.lock();
        if inner.epoch == epoch {
            return false;
        }
        match inner.events.front() {
            Some(&(oldest, _)) if oldest <= epoch + 1 => {}
            _ => return true,
        }
        inner
            .events
            .iter()
            .any(|(e, t)| *e > epoch && t.matches(key, tags))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(s: &str) -> PurgeTarget {
        PurgeTarget::Tag(s.into())
    }

    #[test]
    fn nothing_purged_at_the_same_epoch() {
        let log = PurgeLog::new(8);
        let e = log.epoch();
        assert!(!log.purged_since(e, "k", &[]));
    }

    #[test]
    fn matching_purges_are_found() {
        let log = PurgeLog::new(8);
        let start = log.epoch();
        log.record(&[tag("a"), PurgeTarget::Key("host/x".into())]);
        assert!(log.purged_since(start, "host/y", &["a".into()]));
        assert!(log.purged_since(start, "host/x", &[]));
        assert!(!log.purged_since(start, "host/y", &["b".into()]));
        let later = log.epoch();
        assert!(!log.purged_since(later, "host/x", &[]));
    }

    #[test]
    fn lost_history_is_treated_as_purged() {
        let log = PurgeLog::new(2);
        let start = log.epoch();
        for i in 0..5 {
            log.record(&[tag(&format!("t{i}"))]);
        }
        assert!(log.purged_since(start, "k", &[]));
        let recent = log.epoch() - 1;
        assert!(!log.purged_since(recent, "k", &[]));
    }

    #[test]
    fn empty_purge_advances_the_epoch_only() {
        let log = PurgeLog::new(0);
        let start = log.epoch();
        assert_eq!(log.record(&[]), start + 1);
        assert!(log.purged_since(start, "k", &[]));
    }
}
