//! Connection admission: the limits a listener checks when it accepts a
//! connection, before reading any byte from it. Ported from pacerd's
//! `admit` and `pd_conn_accept` (`proxy.c`), its per-listener state
//! (`daemon.c`) and its counters (`daemon.h`, `admin.c`).
//!
//! [`Admission`] is one listener's shared state. Every worker that accepts
//! for the listener calls [`Admission::admit`] with the peer address, and
//! the checks run in pacerd's order:
//!
//! 1. **The listener's cap.** At most `max_conns` connections are open at
//!    once across every worker, counted in one atomic. A connection over
//!    the cap is refused as [`Refusal::ListenerFull`].
//! 2. **The client's cap and rate, in one table call.** The client is its
//!    address, with IPv6 addresses grouped by `ipv6_prefix` and
//!    IPv4-mapped IPv6 addresses taken as IPv4 ([`ClientKey`]). One
//!    [`Table::hold`] both checks that the client has fewer than
//!    `per_ip_max_conns` open and takes one token of `per_ip_conn_rate`,
//!    so a refusal for either takes nothing:
//!    [`Refusal::PerIpConns`] at the cap, [`Refusal::PerIpRate`] when the
//!    rate has no token, and [`Refusal::TableFull`] when the client is new
//!    and the table has no room for it under [`FullPolicy::Deny`]. With
//!    neither limit set the table is not touched at all.
//!
//! An admitted connection gets a [`Permit`]. Dropping it gives back the
//! client's hold and then the listener slot, in that order, so a client
//! that sees its connection close and reconnects at once is admitted on
//! any worker, as in pacerd.
//!
//! [`Admission::reconfigure`] applies a reloaded listener in place: the
//! caps, the IPv6 prefix and the rate change for the next accept and keep
//! every open connection's hold and every client's debt. The table's size
//! and full policy are fixed memory and cannot change without a restart,
//! and a reload that changes them is refused with pacerd's message.
//!
//! [`Counters`] are pacerd's per-listener stats, with pacerd's names
//! ([`Stat::name`]): `admit` counts each acceptance and each refusal under
//! its reason, and the data path counts closes through [`Stat::for_reason`]
//! with the same reason strings pacerd logs.

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::config::ListenerConfig;
use crate::gcra::{Error, Rate};
use crate::table::{FullPolicy, Table, TableConfig, TableStats};

/// The longest [`ClientKey`]: a whole IPv6 address.
pub const CLIENT_KEY_MAX: usize = 16;

/// A client as the per-address limits see it: the 4 bytes of an IPv4
/// address, or 16 bytes of an IPv6 address with the bits past the
/// listener's prefix cleared. An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`)
/// is the IPv4 client, so a dual-stack listener counts it once. A port of
/// pacerd's `pd_client_key`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientKey {
    bytes: [u8; CLIENT_KEY_MAX],
    len: u8,
}

impl ClientKey {
    /// The key of `ip` with IPv6 addresses grouped by their first
    /// `ipv6_prefix` bits. A prefix over 128 is 128.
    pub fn new(ip: IpAddr, ipv6_prefix: u32) -> Self {
        let mut bytes = [0u8; CLIENT_KEY_MAX];
        let v6 = match ip {
            IpAddr::V4(v4) => {
                bytes[..4].copy_from_slice(&v4.octets());
                return Self { bytes, len: 4 };
            }
            IpAddr::V6(v6) => v6,
        };
        if let Some(v4) = v6.to_ipv4_mapped() {
            bytes[..4].copy_from_slice(&v4.octets());
            return Self { bytes, len: 4 };
        }
        let a = v6.octets();
        let bits = ipv6_prefix.min(128) as usize;
        bytes[..bits / 8].copy_from_slice(&a[..bits / 8]);
        if !bits.is_multiple_of(8) {
            bytes[bits / 8] = a[bits / 8] & (0xff00u16 >> (bits % 8)) as u8;
        }
        Self {
            bytes,
            len: CLIENT_KEY_MAX as u8,
        }
    }

    /// The key's bytes, as the client table stores them.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl fmt::Debug for ClientKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClientKey(")?;
        for b in self.as_bytes() {
            write!(f, "{b:02x}")?;
        }
        f.write_str(")")
    }
}

/// Why a connection was refused at accept. [`Refusal::as_str`] is the
/// `reason=` pacerd logs for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// The listener already has `max_conns` connections open.
    ListenerFull,
    /// The client already has `per_ip_max_conns` connections open.
    PerIpConns,
    /// The client's `per_ip_conn_rate` has no token left.
    PerIpRate,
    /// The client is new and the client table has no room for it.
    TableFull,
    /// The process ran out of file descriptors; the connection was
    /// accepted on a spare descriptor and closed at once.
    FdExhausted,
    /// The process ran out of file descriptors and had no spare, so it
    /// stopped accepting for a moment.
    FdExhaustedPaused,
}

impl Refusal {
    /// Every refusal, in pacerd's order.
    pub const ALL: [Refusal; 6] = [
        Refusal::ListenerFull,
        Refusal::PerIpConns,
        Refusal::PerIpRate,
        Refusal::TableFull,
        Refusal::FdExhausted,
        Refusal::FdExhaustedPaused,
    ];

    /// pacerd's `reason=` for this refusal.
    pub const fn as_str(self) -> &'static str {
        match self {
            Refusal::ListenerFull => "listener_full",
            Refusal::PerIpConns => "per_ip_conns",
            Refusal::PerIpRate => "per_ip_rate",
            Refusal::TableFull => "table_full",
            Refusal::FdExhausted => "fd_exhausted",
            Refusal::FdExhaustedPaused => "fd_exhausted_paused",
        }
    }

    /// The counter this refusal is counted under. Both kinds of
    /// descriptor exhaustion share one, as in pacerd.
    pub const fn stat(self) -> Stat {
        match self {
            Refusal::ListenerFull => Stat::RejectedListenerFull,
            Refusal::PerIpConns => Stat::RejectedPerIpConns,
            Refusal::PerIpRate => Stat::RejectedPerIpRate,
            Refusal::TableFull => Stat::RejectedTableFull,
            Refusal::FdExhausted | Refusal::FdExhaustedPaused => Stat::RejectedFdExhausted,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One of a listener's counters, in the order pacerd's `stats` command
/// prints them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stat {
    Accepted,
    RejectedListenerFull,
    RejectedPerIpConns,
    RejectedPerIpRate,
    RejectedTableFull,
    RejectedFdExhausted,
    ClosedDone,
    ClosedIdle,
    ClosedSlow,
    ClosedUpstreamFailed,
    ClosedReset,
    ClosedDrain,
    ClosedOther,
    BytesUp,
    BytesDown,
    Throttled,
}

impl Stat {
    /// How many counters there are.
    pub const COUNT: usize = 16;

    /// Every counter, in pacerd's order.
    pub const ALL: [Stat; Stat::COUNT] = [
        Stat::Accepted,
        Stat::RejectedListenerFull,
        Stat::RejectedPerIpConns,
        Stat::RejectedPerIpRate,
        Stat::RejectedTableFull,
        Stat::RejectedFdExhausted,
        Stat::ClosedDone,
        Stat::ClosedIdle,
        Stat::ClosedSlow,
        Stat::ClosedUpstreamFailed,
        Stat::ClosedReset,
        Stat::ClosedDrain,
        Stat::ClosedOther,
        Stat::BytesUp,
        Stat::BytesDown,
        Stat::Throttled,
    ];

    /// The name pacerd's `stats` command gives this counter
    /// (`pd_stat_names`).
    pub const fn name(self) -> &'static str {
        match self {
            Stat::Accepted => "accepted",
            Stat::RejectedListenerFull => "rejected_listener_full",
            Stat::RejectedPerIpConns => "rejected_per_ip_conns",
            Stat::RejectedPerIpRate => "rejected_per_ip_rate",
            Stat::RejectedTableFull => "rejected_table_full",
            Stat::RejectedFdExhausted => "rejected_fd_exhausted",
            Stat::ClosedDone => "closed_done",
            Stat::ClosedIdle => "closed_idle",
            Stat::ClosedSlow => "closed_slow",
            Stat::ClosedUpstreamFailed => "closed_upstream_failed",
            Stat::ClosedReset => "closed_reset",
            Stat::ClosedDrain => "closed_drain",
            Stat::ClosedOther => "closed_other",
            Stat::BytesUp => "bytes_up",
            Stat::BytesDown => "bytes_down",
            Stat::Throttled => "throttled",
        }
    }

    /// The counter a close or refusal `reason` belongs to, as pacerd's
    /// `stat_for` maps it: a reason it does not know is
    /// [`Stat::ClosedOther`].
    pub fn for_reason(reason: &str) -> Stat {
        match reason {
            "done" => Stat::ClosedDone,
            "idle_timeout" => Stat::ClosedIdle,
            "slow_client" => Stat::ClosedSlow,
            "connect_timeout" | "upstream_refused" | "upstream_unreachable" | "upstream_socket" => {
                Stat::ClosedUpstreamFailed
            }
            "client_reset" | "client_error" | "upstream_reset" | "upstream_error" => {
                Stat::ClosedReset
            }
            "drain_timeout" | "shutdown" => Stat::ClosedDrain,
            _ => Refusal::ALL
                .into_iter()
                .find(|r| r.as_str() == reason)
                .map_or(Stat::ClosedOther, Refusal::stat),
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// A listener's counters. Every worker adds to the same counters with
/// relaxed atomic adds, and readers see each counter's latest value; a
/// snapshot is not taken at one instant across counters, as pacerd's sum
/// over workers was not.
#[derive(Debug, Default)]
pub struct Counters {
    values: [AtomicU64; Stat::COUNT],
}

impl Counters {
    /// Add `n` to `stat`.
    #[inline]
    pub fn add(&self, stat: Stat, n: u64) {
        self.values[stat.index()].fetch_add(n, Ordering::Relaxed);
    }

    /// Count one close or refusal under its `reason`, as pacerd's
    /// `pd_count` does.
    pub fn count(&self, reason: &str) {
        self.add(Stat::for_reason(reason), 1);
    }

    /// The current value of `stat`.
    pub fn get(&self, stat: Stat) -> u64 {
        self.values[stat.index()].load(Ordering::Relaxed)
    }

    /// Every counter with its name, in pacerd's order.
    pub fn snapshot(&self) -> [(&'static str, u64); Stat::COUNT] {
        Stat::ALL.map(|s| (s.name(), self.get(s)))
    }
}

/// What a limit that is switched off is set to in the client table, so a
/// reload can switch it on in place. pacerd's `rate_or_unlimited`.
const UNLIMITED: Rate = Rate::new(1_000_000_000, 1_000_000_000, 1_000_000_000);

/// `rate_on` in [`Admission::client`].
const RATE_ON: u32 = 1 << 8;

/// One listener's admission state, shared by every worker accepting for it.
/// See the [module docs](self).
pub struct Admission {
    name: String,
    /// `max_conns` in the high half, `per_ip_max_conns` in the low half, so
    /// an accept reads both caps from one reload.
    caps: AtomicU64,
    /// `ipv6_prefix` in the low byte, then [`RATE_ON`].
    client: AtomicU32,
    /// Connections open across every worker.
    open: AtomicU32,
    /// Per-client holds and new-connection rate.
    clients: Table,
    table_size: u32,
    table_full: FullPolicy,
    counters: Counters,
}

impl fmt::Debug for Admission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (max, per_ip) = self.caps();
        f.debug_struct("Admission")
            .field("name", &self.name)
            .field("max_conns", &max)
            .field("per_ip_max_conns", &per_ip)
            .field("open", &self.open())
            .field("clients", &self.clients.stats())
            .finish_non_exhaustive()
    }
}

fn pack_caps(l: &ListenerConfig) -> u64 {
    (u64::from(l.max_conns) << 32) | u64::from(l.per_ip_max_conns)
}

fn pack_client(l: &ListenerConfig) -> u32 {
    let rate_on = if l.per_ip_conn_rate.is_some() {
        RATE_ON
    } else {
        0
    };
    l.ipv6_prefix.min(128) | rate_on
}

impl Admission {
    /// The admission state for listener `l`, with a client table of
    /// `l.table_size` keys allocated now and never grown. Fails with
    /// [`Error::InvalidConfig`] for a table size the table refuses, or
    /// [`Error::InvalidRate`] for a rate it refuses. A config that passed
    /// [`crate::LimiterConfig::validate`] never fails.
    pub fn new(l: &ListenerConfig) -> Result<Self, Error> {
        let mut tc = TableConfig::new(l.table_size, l.per_ip_conn_rate.unwrap_or(UNLIMITED));
        tc.full_policy = l.table_full;
        Ok(Self {
            name: l.name.clone(),
            caps: AtomicU64::new(pack_caps(l)),
            client: AtomicU32::new(pack_client(l)),
            open: AtomicU32::new(0),
            clients: Table::new(&tc)?,
            table_size: l.table_size,
            table_full: l.table_full,
            counters: Counters::default(),
        })
    }

    /// Apply a reloaded config for this listener. The caps, the IPv6
    /// prefix and the rate take effect for the next accept; open
    /// connections keep their holds and clients keep their debt. Refused,
    /// with nothing changed, when `table_size` or `table_full` differs,
    /// with pacerd's message (`listener web: table_size cannot change
    /// without a restart`), or when the rate is invalid.
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
        self.clients
            .reconfigure(&l.per_ip_conn_rate.unwrap_or(UNLIMITED))
            .map_err(|e| format!("listener {}: per_ip_conn_rate: {e}", self.name))?;
        self.caps.store(pack_caps(l), Ordering::Relaxed);
        self.client.store(pack_client(l), Ordering::Relaxed);
        Ok(())
    }

    fn caps(&self) -> (u32, u32) {
        let c = self.caps.load(Ordering::Relaxed);
        ((c >> 32) as u32, c as u32)
    }

    /// Decide whether to admit a connection from `peer` at `now_ns`, and
    /// count the outcome. On a refusal nothing is taken: no listener slot,
    /// no hold and no token.
    pub fn admit(self: &Arc<Self>, peer: IpAddr, now_ns: u64) -> Result<Permit, Refusal> {
        match self.try_admit(peer, now_ns) {
            Ok(permit) => {
                self.counters.add(Stat::Accepted, 1);
                Ok(permit)
            }
            Err(r) => {
                self.counters.add(r.stat(), 1);
                Err(r)
            }
        }
    }

    fn try_admit(self: &Arc<Self>, peer: IpAddr, now_ns: u64) -> Result<Permit, Refusal> {
        let (max_conns, per_ip_max) = self.caps();
        let client = self.client.load(Ordering::Relaxed);
        let key = ClientKey::new(peer, client & 0xff);
        if self.open.fetch_add(1, Ordering::Relaxed) >= max_conns {
            self.open.fetch_sub(1, Ordering::Relaxed);
            return Err(Refusal::ListenerFull);
        }
        // From here the slot is taken, so a refusal must give it back,
        // and so must a Permit dropped later.
        let mut permit = Permit {
            admission: Arc::clone(self),
            key,
            held: false,
        };
        if per_ip_max == 0 && client & RATE_ON == 0 {
            return Ok(permit);
        }
        let cap = if per_ip_max == 0 {
            u32::MAX
        } else {
            per_ip_max
        };
        match self.clients.hold(key.as_bytes(), now_ns, cap) {
            Ok(d) if d.allowed => {
                permit.held = true;
                Ok(permit)
            }
            Ok(_) => Err(Refusal::PerIpRate),
            Err(Error::HoldLimit) => Err(Refusal::PerIpConns),
            // Only TableFull is left: a ClientKey is 4 or 16 bytes, never
            // an invalid key.
            Err(_) => Err(Refusal::TableFull),
        }
    }

    /// Count a refusal decided outside [`Admission::admit`], such as
    /// running out of file descriptors.
    pub fn refuse(&self, r: Refusal) {
        self.counters.add(r.stat(), 1);
    }

    /// The listener's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Connections open now across every worker.
    pub fn open(&self) -> u32 {
        self.open.load(Ordering::Relaxed)
    }

    /// The listener's counters.
    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// The client table's counters: clients tracked, capacity, evictions
    /// and the rest, as pacerd's `stats` reports them.
    pub fn client_stats(&self) -> TableStats {
        self.clients.stats()
    }

    /// Bytes allocated for the client table.
    pub fn memory(&self) -> usize {
        self.clients.memory()
    }

    /// Run a round of idle-client removal over `nshards` shards of the
    /// client table; see [`Table::sweep`].
    pub fn sweep(&self, now_ns: u64, nshards: u32) -> usize {
        self.clients.sweep(now_ns, nshards)
    }

    /// Holds a client has open, and its connection rate's arrival time,
    /// or `None` for a client the table does not track.
    pub fn client(&self, key: &ClientKey) -> Option<crate::table::EntryState> {
        self.clients.get(key.as_bytes())
    }
}

/// One admitted connection's share of its listener's limits. Dropping it
/// gives back the client's hold, then the listener slot.
pub struct Permit {
    admission: Arc<Admission>,
    key: ClientKey,
    held: bool,
}

impl Permit {
    /// The client the connection was counted against.
    pub fn key(&self) -> ClientKey {
        self.key
    }

    /// Whether the connection holds a slot in the client table.
    pub fn held(&self) -> bool {
        self.held
    }

    /// The listener the connection was admitted to.
    pub fn admission(&self) -> &Arc<Admission> {
        &self.admission
    }
}

impl fmt::Debug for Permit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Permit")
            .field("listener", &self.admission.name)
            .field("key", &self.key)
            .field("held", &self.held)
            .finish()
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if self.held {
            // Holds are never evicted or reclaimed, so this cannot fail;
            // if it did, the books would be off and pacerd logged it.
            let released = self.admission.clients.release(self.key.as_bytes());
            debug_assert!(released.is_ok(), "client table lost a hold");
        }
        self.admission.open.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
