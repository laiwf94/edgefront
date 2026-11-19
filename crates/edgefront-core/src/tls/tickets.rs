//! Session ticket keys that rotate on a fixed period and outlive reloads.
//!
//! A TLS 1.3 session ticket (and a TLS 1.2 one) is the server's state
//! encrypted under a key only the server holds; a client that presents it
//! resumes without a full handshake. [`TicketKeys`] keeps two keys: the
//! current one encrypts every new ticket, and the previous one still
//! decrypts, so a ticket issued just before a rotation resumes after it.
//! Every `period` (an hour by default) the current key becomes the
//! previous one, a fresh key becomes current, and the old previous key is
//! dropped, so no ticket stays usable for more than two periods and a
//! leaked key exposes at most two periods of tickets.
//!
//! The keys live outside any one [`Snapshot`](super::Snapshot): the
//! listener hands the same `Arc<TicketKeys>` to every snapshot it loads,
//! so a reload neither invalidates the tickets clients hold nor resets the
//! rotation. Time comes from an injected [`Clock`], checked on every
//! encrypt and decrypt, so tests can drive rotations without waiting.
//!
//! Each key is one of ring's AEAD ticketers from rustls (ChaCha20-Poly1305
//! with a random key and a key name, made by
//! `rustls::crypto::ring::Ticketer::new`); this module only decides which
//! keys are live. Those ticketers rotate themselves only after six hours,
//! and none here lives past two periods, so with periods up to three hours
//! the rotation is entirely this module's.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustls::server::ProducesTickets;

use crate::clock::Clock;

/// The rotation period unless a listener sets another.
pub const DEFAULT_PERIOD: Duration = Duration::from_secs(3600);

/// The longest period: two periods must fit inside the six hours after
/// which ring's ticketers would start rotating on their own.
pub const MAX_PERIOD: Duration = Duration::from_secs(3 * 3600);

/// Makes one ticket key.
pub type KeyMaker = Box<dyn Fn() -> Result<Arc<dyn ProducesTickets>, rustls::Error> + Send + Sync>;

struct Keys {
    current: Arc<dyn ProducesTickets>,
    previous: Option<Arc<dyn ProducesTickets>>,
    /// When `current` became current.
    since: Instant,
    rotations: u64,
}

/// Ticket keys rotated every `period` on a clock; see the module
/// documentation.
pub struct TicketKeys {
    clock: Arc<dyn Clock>,
    period: Duration,
    make: KeyMaker,
    keys: Mutex<Keys>,
}

impl fmt::Debug for TicketKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TicketKeys")
            .field("period", &self.period)
            .field("rotations", &self.rotations())
            .finish_non_exhaustive()
    }
}

/// ring's ticket key, as rustls makes it.
pub fn ring_key() -> Result<Arc<dyn ProducesTickets>, rustls::Error> {
    rustls::crypto::ring::Ticketer::new()
}

impl TicketKeys {
    /// Keys rotated every `period` (at most [`MAX_PERIOD`], at least a
    /// second) on `clock`, made by ring.
    pub fn new(clock: Arc<dyn Clock>, period: Duration) -> Result<Arc<TicketKeys>, rustls::Error> {
        TicketKeys::with_maker(clock, period, Box::new(ring_key))
    }

    /// Like [`new`](Self::new), with keys made by `make`.
    pub fn with_maker(
        clock: Arc<dyn Clock>,
        period: Duration,
        make: KeyMaker,
    ) -> Result<Arc<TicketKeys>, rustls::Error> {
        let period = period.clamp(Duration::from_secs(1), MAX_PERIOD);
        let current = make()?;
        let since = clock.now();
        Ok(Arc::new(TicketKeys {
            clock,
            period,
            make,
            keys: Mutex::new(Keys {
                current,
                previous: None,
                since,
                rotations: 0,
            }),
        }))
    }

    /// The rotation period.
    pub fn period(&self) -> Duration {
        self.period
    }

    /// Rotations so far.
    pub fn rotations(&self) -> u64 {
        self.lock().rotations
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Keys> {
        self.keys.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The live keys after any rotation that has come due. A clock that
    /// jumped more than a period ahead rotates once per period passed, so
    /// after two or more nothing issued before survives.
    fn live(&self) -> (Arc<dyn ProducesTickets>, Option<Arc<dyn ProducesTickets>>) {
        let now = self.clock.now();
        let mut keys = self.lock();
        while now.saturating_duration_since(keys.since) >= self.period {
            let fresh = match (self.make)() {
                Ok(k) => k,
                Err(e) => {
                    // Keep serving on the current key rather than fail
                    // handshakes; the next call tries again.
                    tracing::warn!(error = %e, "cannot make a session ticket key");
                    break;
                }
            };
            let old = std::mem::replace(&mut keys.current, fresh);
            keys.previous = Some(old);
            keys.since += self.period;
            keys.rotations += 1;
            tracing::debug!(rotations = keys.rotations, "session ticket keys rotated");
        }
        (Arc::clone(&keys.current), keys.previous.clone())
    }
}

impl ProducesTickets for TicketKeys {
    fn enabled(&self) -> bool {
        true
    }

    /// The lifetime clients are told: one period, after which the ticket
    /// may already be one rotation old.
    fn lifetime(&self) -> u32 {
        u32::try_from(self.period.as_secs()).unwrap_or(u32::MAX)
    }

    fn encrypt(&self, plain: &[u8]) -> Option<Vec<u8>> {
        self.live().0.encrypt(plain)
    }

    fn decrypt(&self, cipher: &[u8]) -> Option<Vec<u8>> {
        let (current, previous) = self.live();
        current
            .decrypt(cipher)
            .or_else(|| previous.and_then(|p| p.decrypt(cipher)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    fn keys(clock: &Arc<ManualClock>) -> Arc<TicketKeys> {
        TicketKeys::new(Arc::clone(clock) as Arc<dyn Clock>, DEFAULT_PERIOD).unwrap()
    }

    #[test]
    fn a_ticket_survives_one_rotation_and_not_two() {
        let clock = Arc::new(ManualClock::new());
        let k = keys(&clock);
        assert!(k.enabled());
        assert_eq!(k.lifetime(), 3600);
        let t = k.encrypt(b"session").unwrap();
        assert_eq!(k.decrypt(&t).as_deref(), Some(&b"session"[..]));
        clock.advance(Duration::from_secs(3599));
        assert_eq!(k.rotations(), 0);
        assert_eq!(k.decrypt(&t).as_deref(), Some(&b"session"[..]));
        clock.advance(Duration::from_secs(1));
        assert_eq!(k.decrypt(&t).as_deref(), Some(&b"session"[..]));
        assert_eq!(k.rotations(), 1);
        let t2 = k.encrypt(b"later").unwrap();
        clock.advance(DEFAULT_PERIOD);
        assert_eq!(k.decrypt(&t), None, "two rotations old");
        assert_eq!(k.decrypt(&t2).as_deref(), Some(&b"later"[..]));
        assert_eq!(k.rotations(), 2);
        assert_eq!(k.decrypt(b"not a ticket"), None);
    }

    #[test]
    fn a_long_jump_rotates_once_per_period() {
        let clock = Arc::new(ManualClock::new());
        let k = keys(&clock);
        let t = k.encrypt(b"x").unwrap();
        clock.advance(DEFAULT_PERIOD * 5 + Duration::from_secs(10));
        assert_eq!(k.decrypt(&t), None);
        assert_eq!(k.rotations(), 5);
        // The schedule stays on the grid: the next rotation is 50 minutes
        // and 50 seconds away, not a full period.
        clock.advance(Duration::from_secs(3590));
        k.encrypt(b"y").unwrap();
        assert_eq!(k.rotations(), 6);
        assert!(format!("{k:?}").contains("rotations: 6"));
    }

    #[test]
    fn periods_are_clamped_and_a_failed_key_keeps_the_current_one() {
        let clock = Arc::new(ManualClock::new());
        let k = TicketKeys::new(Arc::clone(&clock) as Arc<dyn Clock>, Duration::ZERO).unwrap();
        assert_eq!(k.period(), Duration::from_secs(1));
        let k = TicketKeys::new(
            Arc::clone(&clock) as Arc<dyn Clock>,
            Duration::from_secs(86400),
        )
        .unwrap();
        assert_eq!(k.period(), MAX_PERIOD);

        let made = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let m = Arc::clone(&made);
        let k = TicketKeys::with_maker(
            Arc::clone(&clock) as Arc<dyn Clock>,
            DEFAULT_PERIOD,
            Box::new(move || {
                if m.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                    ring_key()
                } else {
                    Err(rustls::Error::General("no randomness".into()))
                }
            }),
        )
        .unwrap();
        let t = k.encrypt(b"kept").unwrap();
        clock.advance(DEFAULT_PERIOD * 3);
        assert_eq!(k.decrypt(&t).as_deref(), Some(&b"kept"[..]));
        assert_eq!(k.rotations(), 0);
        assert!(made.load(std::sync::atomic::Ordering::Relaxed) >= 2);
    }
}
