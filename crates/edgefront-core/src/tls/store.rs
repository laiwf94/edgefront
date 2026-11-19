//! The certificate store: the snapshot new handshakes use, swapped
//! whole on reload.
//!
//! A handshake reads the current snapshot once, when it starts, and uses
//! that snapshot's rustls configuration to the end, so a swap changes
//! what new handshakes see and leaves everything else alone: handshakes
//! already running finish on the snapshot they started with, and
//! established connections keep the certificate and keys they
//! negotiated, as steer's `GetConfigForClient` snapshot did. The snapshot
//! sits behind a read-write lock that is held only to clone or replace
//! the `Arc`, so a reload never waits for a handshake and a handshake
//! never waits for a reload's file reads.

use std::sync::{Arc, RwLock};

use super::listener::Snapshot;

/// Publishes the current [`Snapshot`] to handshakes.
#[derive(Debug)]
pub struct Store {
    current: RwLock<Arc<Snapshot>>,
    swapped: tokio::sync::Notify,
}

impl Store {
    /// A store serving `snapshot`.
    pub fn new(snapshot: Arc<Snapshot>) -> Arc<Store> {
        Arc::new(Store {
            current: RwLock::new(snapshot),
            swapped: tokio::sync::Notify::new(),
        })
    }

    /// Makes `snapshot` serve every handshake that starts from now on.
    pub fn swap(&self, snapshot: Arc<Snapshot>) {
        *self.current.write().unwrap_or_else(|p| p.into_inner()) = snapshot;
        self.swapped.notify_waiters();
    }

    /// Resolves at the next [`swap`](Self::swap) after it is polled or
    /// [enabled](tokio::sync::futures::Notified::enable).
    pub fn swapped(&self) -> tokio::sync::futures::Notified<'_> {
        self.swapped.notified()
    }

    /// The snapshot new handshakes use.
    pub fn current(&self) -> Arc<Snapshot> {
        Arc::clone(&self.current.read().unwrap_or_else(|p| p.into_inner()))
    }
}
