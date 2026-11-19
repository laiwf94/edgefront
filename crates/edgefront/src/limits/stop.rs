//! Ending every open connection at the drain timeout, without a lock on
//! the data path. Each connection holds its own [`Flag`], which its relay
//! polls on every wake: one atomic load, and on the way to sleeping its
//! waker stored behind a lock no other connection shares. [`Stop`] knows
//! every live flag and raises them all at once. A shared channel would put
//! every connection's every wake behind one lock that all worker threads
//! contend for.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

use edgefront_core::l4::CloseReason;

/// One connection's stop flag.
#[derive(Debug, Default)]
pub struct Flag {
    raised: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl Flag {
    fn raise(&self) {
        self.raised.store(true, Ordering::Release);
        let w = self.waker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(w) = w {
            w.wake();
        }
    }
}

/// Every live connection's flag, and whether the stop has happened.
#[derive(Debug, Default)]
pub struct Stop {
    raised: AtomicBool,
    next: AtomicU64,
    flags: Mutex<BTreeMap<u64, Weak<Flag>>>,
}

impl Stop {
    /// A flag for a new connection, already raised after a stop. The
    /// returned guard takes it out of the set when dropped.
    pub fn flag(self: &Arc<Self>) -> Stopped {
        let flag = Arc::new(Flag::default());
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.flags
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Arc::downgrade(&flag));
        if self.raised.load(Ordering::Acquire) {
            flag.raise();
        }
        Stopped {
            stop: Arc::clone(self),
            id,
            flag,
            last: None,
        }
    }

    /// Raises every flag: each connection's relay ends at its next poll.
    pub fn raise(&self) {
        self.raised.store(true, Ordering::Release);
        let flags: Vec<Arc<Flag>> = self
            .flags
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        for f in flags {
            f.raise();
        }
    }

    /// Connections holding a flag now.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.flags.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// A connection's view of the stop: a future that completes with
/// [`CloseReason::DrainTimeout`] once its flag is raised.
#[derive(Debug)]
pub struct Stopped {
    stop: Arc<Stop>,
    id: u64,
    flag: Arc<Flag>,
    /// The waker last stored in the flag: while the task's waker stays the
    /// same, a poll takes no lock at all.
    last: Option<Waker>,
}

impl Future for Stopped {
    type Output = CloseReason;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<CloseReason> {
        if self.flag.raised.load(Ordering::Acquire) {
            return Poll::Ready(CloseReason::DrainTimeout);
        }
        if !self.last.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
            let w = cx.waker().clone();
            *self.flag.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(w.clone());
            self.last = Some(w);
        }
        // A raise between the load and storing the waker found no waker;
        // look again.
        if self.flag.raised.load(Ordering::Acquire) {
            return Poll::Ready(CloseReason::DrainTimeout);
        }
        Poll::Pending
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        self.stop
            .flags
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn raising_ends_every_waiting_connection_and_any_new_one() {
        let stop = Arc::new(Stop::default());
        let a = stop.flag();
        let b = stop.flag();
        assert_eq!(stop.len(), 2);
        let ta = tokio::spawn(a);
        let tb = tokio::spawn(b);
        tokio::task::yield_now().await;
        assert!(!ta.is_finished() && !tb.is_finished());
        stop.raise();
        assert_eq!(ta.await.unwrap(), CloseReason::DrainTimeout);
        assert_eq!(tb.await.unwrap(), CloseReason::DrainTimeout);
        assert_eq!(stop.len(), 0);
        // A connection made after the stop is stopped already.
        assert_eq!(stop.flag().await, CloseReason::DrainTimeout);
    }

    #[test]
    fn a_dropped_connection_leaves_the_set() {
        let stop = Arc::new(Stop::default());
        let a = stop.flag();
        drop(stop.flag());
        assert_eq!(stop.len(), 1);
        drop(a);
        assert_eq!(stop.len(), 0);
        stop.raise();
    }
}
