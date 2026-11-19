//! What the admin API reports about a listener: pacerd's `stats` counters
//! (`pd_stats_render` in `admin.c`), and the clients behind them, which
//! pacerctl could not show: the busiest clients by open connections, and
//! one address's limits as they stand.
//!
//! Every function here reads; none changes a table entry, its recency or a
//! counter. Lookups go through [`Table::get`](crate::Table::get) and
//! [`Table::entries`](crate::Table::entries), which lock one shard at a
//! time.

use std::fmt::{self, Write};
use std::net::IpAddr;

use crate::conn::{Admission, ClientKey, Stat};
use crate::shape::Bandwidth;
use crate::table::TableStats;

/// One listener's counters and table sizes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenerStats {
    pub name: String,
    /// Connections open now.
    pub open: u32,
    /// pacerd's 16 counters in its order, then `rejected_per_ip_quota`.
    pub counters: [(&'static str, u64); Stat::COUNT],
    /// The client table (connection caps and rate).
    pub clients: TableStats,
    /// The per-client byte table.
    pub byte_clients: TableStats,
    /// The quota table, for a listener with a quota.
    pub quota: Option<TableStats>,
    /// Bytes allocated for the listener's tables.
    pub memory: usize,
}

impl ListenerStats {
    /// The listener's stats now.
    pub fn of(a: &Admission, b: &Bandwidth) -> Self {
        Self {
            name: a.name().to_string(),
            open: a.open(),
            counters: a.counters().snapshot(),
            clients: a.client_stats(),
            byte_clients: b.client_stats(),
            quota: a.quota_stats(),
            memory: a.memory() + b.memory(),
        }
    }

    /// A counter by its name, as pacerd's `stats` names it.
    pub fn get(&self, name: &str) -> Option<u64> {
        if name == "open" {
            return Some(u64::from(self.open));
        }
        self.counters
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| *v)
    }
}

impl fmt::Display for ListenerStats {
    /// pacerd's stats line: `listener=NAME open=N` and every counter, then
    /// the client table's size and evictions.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "listener={} open={}", self.name, self.open)?;
        for (name, v) in &self.counters {
            write!(f, " {name}={v}")?;
        }
        write!(
            f,
            " clients_tracked={} client_table_capacity={} client_table_evictions={}",
            self.clients.entries, self.clients.capacity, self.clients.evictions
        )
    }
}

/// One client as the limits see it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientInfo {
    /// The key: the IPv4 address, or the IPv6 address cut to the prefix.
    pub key: ClientKey,
    /// The IPv6 prefix the key was cut to (shown for IPv6 keys).
    pub prefix: u32,
    /// Whether the client table tracks the client.
    pub tracked: bool,
    /// Connections it has open.
    pub open: u32,
    /// New-connection tokens left, with `per_ip_conn_rate` on.
    pub conn_tokens: Option<u64>,
    /// Byte tokens left, with `per_ip_bandwidth` on.
    pub byte_tokens: Option<u64>,
    /// Connections opened in the quota's window, with a quota on and the
    /// client tracked by the quota table.
    pub quota_used: Option<u64>,
}

fn opt(v: Option<u64>) -> String {
    v.map_or_else(|| "-".to_string(), |v| v.to_string())
}

impl ClientInfo {
    /// The key as text: `192.0.2.1`, or `2001:db8:1:2::/64`.
    pub fn key_text(&self) -> String {
        match self.key.ip() {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => format!("{v6}/{}", self.prefix),
        }
    }
}

impl fmt::Display for ClientInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "client={} tracked={} open={} conn_tokens={} byte_tokens={} quota_used={}",
            self.key_text(),
            if self.tracked { "yes" } else { "no" },
            self.open,
            opt(self.conn_tokens),
            opt(self.byte_tokens),
            opt(self.quota_used),
        )
    }
}

fn info(a: &Admission, b: &Bandwidth, key: ClientKey, now_ns: u64) -> ClientInfo {
    let st = a.client(&key);
    ClientInfo {
        key,
        prefix: a.ipv6_prefix(),
        tracked: st.is_some(),
        open: st.map_or(0, |s| s.holds),
        conn_tokens: a.conn_tokens(&key, now_ns),
        byte_tokens: b.client_tokens(&key, now_ns),
        quota_used: a.quota_count(&key, now_ns),
    }
}

/// The client `ip` belongs to under the listener's IPv6 prefix, at
/// `now_ns`.
pub fn client(a: &Admission, b: &Bandwidth, ip: IpAddr, now_ns: u64) -> ClientInfo {
    info(a, b, a.key_of(ip), now_ns)
}

/// The `n` tracked clients with the most connections open, most first,
/// ties in key order.
pub fn top_clients(a: &Admission, b: &Bandwidth, n: usize, now_ns: u64) -> Vec<ClientInfo> {
    let mut all = a.clients();
    all.sort_by(|(ka, sa), (kb, sb)| {
        sb.holds
            .cmp(&sa.holds)
            .then_with(|| ka.as_bytes().cmp(kb.as_bytes()))
    });
    all.into_iter()
        .take(n)
        .map(|(k, _)| info(a, b, k, now_ns))
        .collect()
}

/// The text of [`top_clients`] for the admin API: a header line, then one
/// line per client.
pub fn render_top(a: &Admission, b: &Bandwidth, n: usize, now_ns: u64) -> String {
    let top = top_clients(a, b, n, now_ns);
    let mut out = format!(
        "listener={} clients_tracked={} shown={}\n",
        a.name(),
        a.client_stats().entries,
        top.len()
    );
    for c in top {
        let _ = writeln!(out, "{c}");
    }
    out
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use super::*;
    use crate::config::{ConnQuota, ListenerConfig};
    use crate::gcra::Rate;

    const SEC: u64 = 1_000_000_000;

    fn listener(edit: impl FnOnce(&mut ListenerConfig)) -> (Arc<Admission>, Bandwidth) {
        let a: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let mut l = ListenerConfig::new("web", a, a);
        edit(&mut l);
        (
            Arc::new(Admission::new(&l).unwrap()),
            Bandwidth::new(&l).unwrap(),
        )
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_stats_line_is_pacerds() {
        let (a, b) = listener(|l| l.per_ip_max_conns = 1);
        let _p = a.admit(ip("192.0.2.1"), SEC).unwrap();
        assert!(a.admit(ip("192.0.2.1"), SEC).is_err());
        let s = ListenerStats::of(&a, &b);
        assert_eq!(s.get("open"), Some(1));
        assert_eq!(s.get("accepted"), Some(1));
        assert_eq!(s.get("rejected_per_ip_conns"), Some(1));
        assert_eq!(s.get("nope"), None);
        assert_eq!(s.quota, None);
        assert!(s.memory >= a.memory());
        let line = s.to_string();
        assert!(line.starts_with(
            "listener=web open=1 accepted=1 rejected_listener_full=0 rejected_per_ip_conns=1 "
        ));
        assert!(line.ends_with(" rejected_per_ip_quota=0 clients_tracked=1 client_table_capacity=16384 client_table_evictions=0"));
    }

    #[test]
    fn a_client_lookup_reads_every_limit_without_changing_any() {
        let (a, b) = listener(|l| {
            l.per_ip_conn_rate = Some(Rate::new(10, SEC, 10));
            l.per_ip_bandwidth = Some(Rate::new(1000, SEC, 1000));
            l.per_ip_conn_quota = Some(ConnQuota {
                limit: 100,
                window: std::time::Duration::from_secs(3600),
            });
        });
        let p1 = a.admit(ip("192.0.2.1"), SEC).unwrap();
        let _p2 = a.admit(ip("192.0.2.1"), SEC).unwrap();
        let c = client(&a, &b, ip("192.0.2.1"), SEC);
        assert_eq!(
            c.to_string(),
            "client=192.0.2.1 tracked=yes open=2 conn_tokens=8 byte_tokens=1000 quota_used=2"
        );
        // Asking again changes nothing.
        assert_eq!(client(&a, &b, ip("192.0.2.1"), SEC), c);
        drop(p1);
        assert_eq!(client(&a, &b, ip("192.0.2.1"), SEC).open, 1);
        // An unknown client: not tracked, with full buckets.
        let n = client(&a, &b, ip("198.51.100.7"), SEC);
        assert_eq!(
            n.to_string(),
            "client=198.51.100.7 tracked=no open=0 conn_tokens=10 byte_tokens=1000 quota_used=-"
        );
        assert_eq!(a.client_stats().entries, 1);
    }

    #[test]
    fn limits_that_are_off_read_as_a_dash_and_ipv6_shows_its_prefix() {
        let (a, b) = listener(|l| l.ipv6_prefix = 48);
        let _p = a.admit(ip("2001:db8:1:2::9"), SEC);
        let c = client(&a, &b, ip("2001:db8:1:ffff::1"), SEC);
        // Neither cap nor rate is set, so the table is not used at all.
        assert_eq!(
            c.to_string(),
            "client=2001:db8:1::/48 tracked=no open=0 conn_tokens=- byte_tokens=- quota_used=-"
        );
        assert_eq!(c.key_text(), "2001:db8:1::/48");
    }

    #[test]
    fn the_top_clients_are_the_busiest_first() {
        let (a, b) = listener(|l| l.per_ip_max_conns = 10);
        let mut held = Vec::new();
        for (who, n) in [
            ("192.0.2.3", 1),
            ("192.0.2.1", 3),
            ("192.0.2.2", 3),
            ("2001:db8::1", 2),
        ] {
            for _ in 0..n {
                held.push(a.admit(ip(who), SEC).unwrap());
            }
        }
        let top: Vec<String> = top_clients(&a, &b, 3, SEC)
            .iter()
            .map(|c| format!("{} {}", c.key_text(), c.open))
            .collect();
        assert_eq!(top, ["192.0.2.1 3", "192.0.2.2 3", "2001:db8::/64 2"]);
        let text = render_top(&a, &b, 10, SEC);
        assert!(
            text.starts_with("listener=web clients_tracked=4 shown=4\n"),
            "{text}"
        );
        assert_eq!(text.lines().count(), 5);
        assert!(text.contains("\nclient=192.0.2.3 tracked=yes open=1 "));
        assert_eq!(top_clients(&a, &b, 0, SEC), []);
    }

    #[test]
    fn keys_round_trip_through_their_bytes() {
        for s in ["192.0.2.1", "2001:db8::"] {
            let k = ClientKey::new(ip(s), 64);
            assert_eq!(ClientKey::from_bytes(k.as_bytes()), Some(k));
            assert_eq!(k.ip(), ip(s));
        }
        assert_eq!(ClientKey::from_bytes(&[1, 2, 3]), None);
        assert_eq!(ClientKey::from_bytes(&[]), None);
    }
}
