//! Request collapsing: concurrent misses for one key share one origin fetch.
//!
//! The fetch runs on its own task. A client that gives up does not cancel
//! it for everyone else, and its result still reaches the cache.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;

use crate::object::Key;

/// Whether this caller started the fetch or joined one already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Leader,
    Follower,
}

/// The fetch ended without a result (it panicked).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Abandoned;

type Flights<T> = HashMap<Key, watch::Receiver<Option<T>>>;

pub struct Collapser<T> {
    inflight: Mutex<Flights<T>>,
}

impl<T> Default for Collapser<T> {
    fn default() -> Self {
        Collapser {
            inflight: Mutex::new(HashMap::new()),
        }
    }
}

impl<T> Collapser<T> {
    fn lock(&self) -> MutexGuard<'_, Flights<T>> {
        self.inflight.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Removes the key when the flight task ends, even by panic.
struct Cleanup<T> {
    collapser: Arc<Collapser<T>>,
    key: Key,
}

impl<T> Drop for Cleanup<T> {
    fn drop(&mut self) {
        self.collapser.lock().remove(&self.key);
    }
}

impl<T: Clone + Send + Sync + 'static> Collapser<T> {
    /// Joins the flight for `key`, starting one with `make` if none is
    /// running. `make` is called under a lock, so it should only build the
    /// future, not do work.
    fn join<F, Fut>(self: &Arc<Self>, key: Key, make: F) -> (watch::Receiver<Option<T>>, Role)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        let mut map = self.lock();
        if let Some(rx) = map.get(&key) {
            return (rx.clone(), Role::Follower);
        }
        let (tx, rx) = watch::channel(None);
        map.insert(key.clone(), rx.clone());
        let fut = make();
        let cleanup = Cleanup {
            collapser: self.clone(),
            key,
        };
        tokio::spawn(async move {
            let value = fut.await;
            // Publish before removing the key, so a caller arriving in
            // between still gets this result instead of a second fetch.
            tx.send_replace(Some(value));
            drop(cleanup);
        });
        (rx, Role::Leader)
    }

    /// Runs `make()` for `key` unless a fetch for it is already running,
    /// then waits for that fetch's result.
    pub async fn run<F, Fut>(self: &Arc<Self>, key: Key, make: F) -> (Result<T, Abandoned>, Role)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        let (mut rx, role) = self.join(key, make);
        let result = match rx.wait_for(Option::is_some).await {
            Ok(value) => Ok(value.clone().expect("waited for Some")),
            Err(_) => Err(Abandoned),
        };
        (result, role)
    }

    /// Starts a fetch for `key` in the background unless one is running.
    /// Returns true when this call started it.
    pub fn spawn<F, Fut>(self: &Arc<Self>, key: Key, make: F) -> bool
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        self.join(key, make).1 == Role::Leader
    }

    /// Number of fetches running now.
    pub fn in_flight(&self) -> usize {
        self.lock().len()
    }

    /// Leads the flight for `key` when none is running, or follows the one
    /// that is. Unlike [`Collapser::run`], the leader makes the fetch
    /// itself, as part of its own request, and hands the result to its
    /// followers with [`Lead::publish`]. A leader dropped before that
    /// releases its followers with [`Abandoned`], and they try again.
    pub fn lead_or_follow(self: &Arc<Self>, key: Key) -> Flight<T> {
        let mut map = self.lock();
        if let Some(rx) = map.get(&key) {
            return Flight::Follow(Follow(rx.clone()));
        }
        let (tx, rx) = watch::channel(None);
        map.insert(key.clone(), rx);
        Flight::Lead(Lead {
            cleanup: Cleanup {
                collapser: self.clone(),
                key,
            },
            tx,
        })
    }
}

/// Whether [`Collapser::lead_or_follow`] made this caller the leader.
pub enum Flight<T> {
    Lead(Lead<T>),
    Follow(Follow<T>),
}

/// The leader's side of a flight.
pub struct Lead<T> {
    // Declared first so it drops first: a leader that goes away without a
    // result frees the key before its followers wake to try again.
    cleanup: Cleanup<T>,
    tx: watch::Sender<Option<T>>,
}

impl<T> Lead<T> {
    /// Hands `value` to every follower, then ends the flight.
    pub fn publish(self, value: T) {
        let Lead { cleanup, tx } = self;
        // Publish before removing the key, so a caller arriving in between
        // still gets this result instead of starting a second fetch.
        tx.send_replace(Some(value));
        drop(cleanup);
    }
}

/// A follower's side of a flight.
pub struct Follow<T>(watch::Receiver<Option<T>>);

impl<T: Clone> Follow<T> {
    /// The leader's result, or [`Abandoned`] when the leader went away
    /// without one.
    pub async fn wait(mut self) -> Result<T, Abandoned> {
        match self.0.wait_for(Option::is_some).await {
            Ok(value) => Ok(value.clone().expect("waited for Some")),
            Err(_) => Err(Abandoned),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::Notify;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_callers_share_one_execution() {
        let c = Arc::new(Collapser::<u32>::default());
        let runs = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Notify::new());
        let mut tasks = Vec::new();
        for _ in 0..100 {
            let (c, runs, gate) = (c.clone(), runs.clone(), gate.clone());
            tasks.push(tokio::spawn(async move {
                c.run("k".into(), move || async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    gate.notified().await;
                    7
                })
                .await
            }));
        }
        while c.in_flight() == 0 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        gate.notify_one();
        let mut leaders = 0;
        for t in tasks {
            let (result, role) = t.await.unwrap();
            assert_eq!(result, Ok(7));
            leaders += usize::from(role == Role::Leader);
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(leaders, 1);
        assert_eq!(c.in_flight(), 0);
    }

    #[tokio::test]
    async fn different_keys_do_not_collapse() {
        let c = Arc::new(Collapser::<&'static str>::default());
        let (a, _) = c.run("a".into(), || async { "a" }).await;
        let (b, _) = c.run("b".into(), || async { "b" }).await;
        assert_eq!((a, b), (Ok("a"), Ok("b")));
    }

    #[tokio::test]
    async fn leader_cancellation_does_not_cancel_the_fetch() {
        let c = Arc::new(Collapser::<u32>::default());
        let gate = Arc::new(Notify::new());
        let g = gate.clone();
        let leader = {
            let c = c.clone();
            tokio::spawn(async move {
                c.run("k".into(), move || async move {
                    g.notified().await;
                    1
                })
                .await
            })
        };
        while c.in_flight() == 0 {
            tokio::task::yield_now().await;
        }
        let follower = {
            let c = c.clone();
            tokio::spawn(async move { c.run("k".into(), || async { 2 }).await })
        };
        leader.abort();
        tokio::time::sleep(Duration::from_millis(10)).await;
        gate.notify_one();
        let (result, role) = follower.await.unwrap();
        assert_eq!(result, Ok(1));
        assert_eq!(role, Role::Follower);
    }

    #[tokio::test]
    async fn panicking_fetch_releases_waiters() {
        let c = Arc::new(Collapser::<u32>::default());
        let (result, role) = c
            .run("k".into(), || async {
                panic!("origin client bug");
            })
            .await;
        assert_eq!(result, Err(Abandoned));
        assert_eq!(role, Role::Leader);
        while c.in_flight() != 0 {
            tokio::task::yield_now().await;
        }
        let (result, _) = c.run("k".into(), || async { 3 }).await;
        assert_eq!(result, Ok(3));
    }

    #[tokio::test]
    async fn a_leader_publishes_to_its_followers() {
        let c = Arc::new(Collapser::<u32>::default());
        let Flight::Lead(lead) = c.lead_or_follow("k".into()) else {
            panic!("the first caller leads");
        };
        let Flight::Follow(f1) = c.lead_or_follow("k".into()) else {
            panic!("the second caller follows");
        };
        let Flight::Follow(f2) = c.lead_or_follow("k".into()) else {
            panic!("so does the third");
        };
        // Another key is its own flight, and `run` joins this one.
        assert!(matches!(c.lead_or_follow("j".into()), Flight::Lead(_)));
        let runner = {
            let c = c.clone();
            tokio::spawn(async move { c.run("k".into(), || async { 9 }).await })
        };
        tokio::task::yield_now().await;
        assert_eq!(c.in_flight(), 1);
        lead.publish(5);
        assert_eq!(c.in_flight(), 0);
        assert_eq!(f1.wait().await, Ok(5));
        assert_eq!(f2.wait().await, Ok(5));
        assert_eq!(runner.await.unwrap(), (Ok(5), Role::Follower));
        assert!(matches!(c.lead_or_follow("k".into()), Flight::Lead(_)));
    }

    #[tokio::test]
    async fn a_dropped_leader_releases_its_followers() {
        let c = Arc::new(Collapser::<u32>::default());
        let Flight::Lead(lead) = c.lead_or_follow("k".into()) else {
            panic!("the first caller leads");
        };
        let Flight::Follow(f) = c.lead_or_follow("k".into()) else {
            panic!("the second caller follows");
        };
        let waiter = tokio::spawn(f.wait());
        tokio::task::yield_now().await;
        drop(lead);
        assert_eq!(waiter.await.unwrap(), Err(Abandoned));
        // The key is free at once, so a follower that retries leads.
        assert_eq!(c.in_flight(), 0);
        assert!(matches!(c.lead_or_follow("k".into()), Flight::Lead(_)));
    }

    #[tokio::test]
    async fn background_spawn_starts_once() {
        let c = Arc::new(Collapser::<u32>::default());
        let gate = Arc::new(Notify::new());
        let g = gate.clone();
        assert!(c.spawn("k".into(), move || async move {
            g.notified().await;
            1
        }));
        assert!(!c.spawn("k".into(), || async { 2 }));
        let waiter = {
            let c = c.clone();
            tokio::spawn(async move { c.run("k".into(), || async { 3 }).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        gate.notify_one();
        assert_eq!(waiter.await.unwrap(), (Ok(1), Role::Follower));
    }
}
