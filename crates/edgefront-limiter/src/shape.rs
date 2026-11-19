//! Byte shaping: how many bytes a connection may read now. Ported from
//! pacerd's `reserve` and `give_back` (`proxy.c`) and the per-listener
//! byte limiters `setup_limits` made (`daemon.c`).
//!
//! A listener has two byte limits, each optional: `per_ip_bandwidth`, one
//! bucket per client in a keyed [`Table`], and `bandwidth`, one [`Gcra`]
//! shared by every connection. [`Bandwidth`] holds both. Before a
//! connection reads from either side it asks [`Shaper::reserve`] for as
//! many bytes as its buffer has room for. The grant is what both limits
//! allow, and when it is 0 the connection leaves the socket unread until
//! [`Reserve::retry_ns`] has passed. The kernel's receive buffer fills,
//! TCP flow control stops the sender, and the proxy never holds more than
//! its one fixed buffer per direction. Bytes granted but not read are
//! handed back with [`Shaper::give_back`], so a short read costs only what
//! it read.
//!
//! Both directions of a connection draw on the same buckets, as in pacerd:
//! a client's `per_ip_bandwidth` covers what it uploads and what it
//! downloads together.
//!
//! What it keeps from pacerd:
//!
//! - **A read quantum.** A reservation is granted only when at least
//!   [`READ_QUANTUM`] bytes (or the whole request, if smaller) are
//!   available. Without it, a connection whose socket is always readable
//!   would take each token the moment it appears, and connections that
//!   wake later would never get one.
//! - **Order.** The client's bucket is asked first. What the listener's
//!   bucket then cannot give is refunded to the client's bucket, so a
//!   client is never charged for bytes the listener held back.
//! - **Waiters on the listener's bucket queue.** Every connection of the
//!   listener shares its bucket. A connection that finds it short books
//!   its next quantum there ([`Gcra::book`]): the bucket goes into debt,
//!   the connection waits until the booking is due and then reads it
//!   without asking again. Bookings queue in the bucket's arrival time, so
//!   waiting connections are served in the order they asked. Without them
//!   every waiter learns the same moment when the next quantum is free,
//!   and whichever runs first takes it, again and again (pacer #238). A
//!   connection gives a booking back when its own per-client limit holds
//!   it back, when the listener's limit is switched off by a reload, and
//!   when it closes ([`Shaper::release`]). The per-client bucket keeps the
//!   quantum only: it is shared by one client's own connections.
//! - **An untracked client waits.** When the client table is full of
//!   active clients under the deny policy, a new client's bytes wait
//!   [`UNTRACKED_RETRY_NS`] and are asked for again; it is never given an
//!   unlimited share.
//! - **Reload in place.** A limit that is off is still a bucket, at
//!   pacerd's unlimited rate, so [`Bandwidth::reconfigure`] can switch it
//!   on or change it without dropping any client's debt.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::config::ListenerConfig;
use crate::conn::{ClientKey, UNLIMITED};
use crate::gcra::{Error, Gcra};
use crate::table::{FullPolicy, Table, TableConfig, TableStats};

/// The smallest read worth waking up for while shaped, in bytes.
pub const READ_QUANTUM: u64 = 4096;

/// How long a client the table cannot track waits before asking again:
/// 50 ms.
pub const UNTRACKED_RETRY_NS: u64 = 50_000_000;

/// `per_ip_bandwidth` is on.
const PER_IP_ON: u32 = 1;
/// `bandwidth` is on.
const LISTENER_ON: u32 = 2;

/// A direction of a connection; each keeps its own booking.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dir {
    /// Client to upstream.
    Up = 0,
    /// Upstream to client.
    Down = 1,
}

/// The answer to a [`Shaper::reserve`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reserve {
    /// Bytes that may be read now. Unread ones go back through
    /// [`Shaper::give_back`].
    pub granted: u64,
    /// When nothing is granted: how long to wait, in nanoseconds, before
    /// asking again. Never 0 when `granted` is 0 and `want` was not.
    pub retry_ns: u64,
}

/// One listener's byte limits, shared by every connection on it. See the
/// [module docs](self).
pub struct Bandwidth {
    name: String,
    /// The bits [`PER_IP_ON`] and [`LISTENER_ON`].
    on: AtomicU32,
    clients: Table,
    listener: Gcra,
    table_size: u32,
    table_full: FullPolicy,
}

impl fmt::Debug for Bandwidth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let on = self.on.load(Ordering::Relaxed);
        f.debug_struct("Bandwidth")
            .field("name", &self.name)
            .field("per_ip_bandwidth", &(on & PER_IP_ON != 0))
            .field("bandwidth", &(on & LISTENER_ON != 0))
            .field("clients", &self.clients.stats())
            .finish_non_exhaustive()
    }
}

fn pack_on(l: &ListenerConfig) -> u32 {
    let mut on = 0;
    if l.per_ip_bandwidth.is_some() {
        on |= PER_IP_ON;
    }
    if l.bandwidth.is_some() {
        on |= LISTENER_ON;
    }
    on
}

/// `want`, or the quantum if that is smaller.
fn quantum(want: u64) -> u64 {
    want.min(READ_QUANTUM)
}

impl Bandwidth {
    /// The byte limits of listener `l`, with a client table of
    /// `l.table_size` keys allocated now, as pacerd allocates it whether
    /// or not `per_ip_bandwidth` is set. Fails as [`Table::new`] and
    /// [`Gcra::new`] do; a config that passed
    /// [`crate::LimiterConfig::validate`] never fails.
    pub fn new(l: &ListenerConfig) -> Result<Self, Error> {
        let mut tc = TableConfig::new(l.table_size, l.per_ip_bandwidth.unwrap_or(UNLIMITED));
        tc.full_policy = l.table_full;
        Ok(Self {
            name: l.name.clone(),
            on: AtomicU32::new(pack_on(l)),
            clients: Table::new(&tc)?,
            listener: Gcra::new(&l.bandwidth.unwrap_or(UNLIMITED))?,
            table_size: l.table_size,
            table_full: l.table_full,
        })
    }

    /// Apply a reloaded config for this listener: both rates change for
    /// the next reservation, and every client keeps its debt. Refused,
    /// with nothing changed, when `table_size` or `table_full` differs
    /// (pacerd's message, as [`crate::Admission::reconfigure`] gives it)
    /// or a rate is invalid.
    pub fn reconfigure(&self, l: &ListenerConfig) -> Result<(), String> {
        let fixed = if l.table_size != self.table_size {
            Some("table_size")
        } else if l.table_full != self.table_full {
            Some("table_full")
        } else {
            None
        };
        if let Some(what) = fixed {
            return Err(format!(
                "listener {}: {what} cannot change without a restart",
                self.name
            ));
        }
        let per_ip = l.per_ip_bandwidth.unwrap_or(UNLIMITED);
        let listener = l.bandwidth.unwrap_or(UNLIMITED);
        // Check both before changing either, so a refusal changes nothing.
        per_ip
            .ticks()
            .map_err(|e| format!("listener {}: per_ip_bandwidth: {e}", self.name))?;
        listener
            .ticks()
            .map_err(|e| format!("listener {}: bandwidth: {e}", self.name))?;
        self.clients.reconfigure(&per_ip).expect("checked rate");
        self.listener.reconfigure(&listener).expect("checked rate");
        self.on.store(pack_on(l), Ordering::Relaxed);
        Ok(())
    }

    /// Whether either limit is on. A connection on a listener with neither
    /// need not ask.
    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed) != 0
    }

    /// Hand back `tokens` granted to `key` and not read.
    pub fn give_back(&self, key: &ClientKey, tokens: u64, now_ns: u64) {
        if tokens == 0 {
            return;
        }
        let on = self.on.load(Ordering::Relaxed);
        if on & PER_IP_ON != 0 {
            self.clients.refund(key.as_bytes(), now_ns, tokens);
        }
        if on & LISTENER_ON != 0 {
            self.listener.refund(now_ns, tokens);
        }
    }

    /// The listener's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Tokens left in the listener's shared bucket at `now_ns`; 0 while
    /// bookings hold it in debt.
    pub fn listener_remaining(&self, now_ns: u64) -> u64 {
        self.listener.remaining(now_ns)
    }

    /// The per-client byte table's counters.
    pub fn client_stats(&self) -> TableStats {
        self.clients.stats()
    }

    /// Bytes allocated for the per-client byte table.
    pub fn memory(&self) -> usize {
        self.clients.memory()
    }

    /// Run a round of idle-client removal over `nshards` shards of the
    /// per-client byte table; see [`Table::sweep`].
    pub fn sweep(&self, now_ns: u64, nshards: u32) -> usize {
        self.clients.sweep(now_ns, nshards)
    }
}

/// One connection's view of its listener's byte limits: the listener's
/// [`Bandwidth`], the client the connection belongs to, and what each
/// direction has booked in the listener's bucket.
#[derive(Debug)]
pub struct Shaper {
    bandwidth: Arc<Bandwidth>,
    key: ClientKey,
    booked: [u64; 2],
}

impl Shaper {
    /// A shaper for a connection from `key` on the listener `bandwidth`
    /// belongs to.
    pub fn new(bandwidth: Arc<Bandwidth>, key: ClientKey) -> Self {
        Self {
            bandwidth,
            key,
            booked: [0; 2],
        }
    }

    /// Whether the listener shapes at all.
    pub fn is_on(&self) -> bool {
        self.bandwidth.is_on()
    }

    /// Reserve up to `want` bytes for `dir` at `now_ns`: from the client's
    /// bucket, then from what `dir` booked in the listener's bucket or the
    /// bucket itself. A grant of 0 comes with the time to wait before
    /// asking again; when the listener's bucket was short, `dir` has booked
    /// its next quantum, due then. pacerd's `reserve`.
    pub fn reserve(&mut self, dir: Dir, want: u64, now_ns: u64) -> Reserve {
        let bw = &self.bandwidth;
        let on = bw.on.load(Ordering::Relaxed);
        let i = dir as usize;
        if on & LISTENER_ON == 0 {
            // Switched off by a reload: the booking went with it.
            self.booked[i] = 0;
        }
        let mut r = Reserve {
            granted: want,
            retry_ns: 0,
        };
        if want == 0 {
            return r;
        }
        if on & PER_IP_ON != 0 {
            match bw
                .clients
                .take_range(self.key.as_bytes(), now_ns, quantum(want), want)
            {
                Ok(d) => {
                    r.granted = d.granted;
                    r.retry_ns = d.retry_after_ns;
                }
                // TableFull: a ClientKey is never an invalid key.
                Err(_) => {
                    self.drop_booking(i, now_ns);
                    return Reserve {
                        granted: 0,
                        retry_ns: UNTRACKED_RETRY_NS,
                    };
                }
            }
            if r.granted == 0 {
                // The client's own limit holds the read back; a booking
                // would keep the listener's tokens from everyone else
                // meanwhile.
                self.drop_booking(i, now_ns);
                return r;
            }
        }
        if on & LISTENER_ON == 0 {
            return r;
        }
        let want = r.granted;
        let g = if self.booked[i] > 0 {
            let g = want.min(self.booked[i]);
            self.booked[i] -= g;
            g
        } else {
            let d = bw.listener.take_range(now_ns, quantum(want), want);
            r.retry_ns = d.retry_after_ns;
            if d.granted == 0 {
                let b = bw.listener.book(now_ns, quantum(want));
                self.booked[i] = b.granted;
                r.retry_ns = b.retry_after_ns;
            }
            d.granted
        };
        if g < want && on & PER_IP_ON != 0 {
            bw.clients.refund(self.key.as_bytes(), now_ns, want - g);
        }
        r.granted = g;
        if g == 0 && r.retry_ns == 0 {
            // A denial always says when to come back.
            r.retry_ns = 1;
        }
        r
    }

    /// Hand back `tokens` reserved and not read.
    pub fn give_back(&self, tokens: u64, now_ns: u64) {
        self.bandwidth.give_back(&self.key, tokens, now_ns);
    }

    /// Tokens `dir` has booked in the listener's bucket and not yet read.
    pub fn booked(&self, dir: Dir) -> u64 {
        self.booked[dir as usize]
    }

    /// Give back both directions' bookings, as a connection does when it
    /// closes.
    pub fn release(&mut self, now_ns: u64) {
        self.drop_booking(Dir::Up as usize, now_ns);
        self.drop_booking(Dir::Down as usize, now_ns);
    }

    fn drop_booking(&mut self, i: usize, now_ns: u64) {
        let n = std::mem::take(&mut self.booked[i]);
        if n > 0 && self.bandwidth.on.load(Ordering::Relaxed) & LISTENER_ON != 0 {
            self.bandwidth.listener.refund(now_ns, n);
        }
    }

    /// The client this connection counts against.
    pub fn key(&self) -> ClientKey {
        self.key
    }
}

#[cfg(test)]
mod tests;
