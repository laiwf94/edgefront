use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;

use super::*;

const SEC: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

fn listener() -> ListenerConfig {
    let a: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let b: SocketAddr = "127.0.0.1:9090".parse().unwrap();
    ListenerConfig::new("web", a, b)
}

fn admission(edit: impl FnOnce(&mut ListenerConfig)) -> Arc<Admission> {
    let mut l = listener();
    edit(&mut l);
    Arc::new(Admission::new(&l).unwrap())
}

// test_util.c: client_key_ipv4
#[test]
fn client_key_ipv4() {
    let k = ClientKey::new(ip("192.0.2.9"), 64);
    assert_eq!(k.as_bytes(), &[0xc0, 0x00, 0x02, 0x09]);
    assert_eq!(format!("{k:?}"), "ClientKey(c0000209)");
}

// test_util.c: client_key_ipv4_mapped_matches_ipv4
#[test]
fn client_key_ipv4_mapped_matches_ipv4() {
    let a = ClientKey::new(ip("::ffff:192.0.2.9"), 64);
    let b = ClientKey::new(ip("192.0.2.9"), 64);
    assert_eq!(a.as_bytes().len(), 4);
    assert_eq!(a, b);
    // The IPv4-compatible form (::a.b.c.d) is an IPv6 address.
    let c = ClientKey::new(
        IpAddr::V6(Ipv4Addr::new(192, 0, 2, 9).to_ipv6_compatible()),
        128,
    );
    assert_eq!(c.as_bytes().len(), 16);
}

// test_util.c: client_key_ipv6_prefix_groups_a_subnet
#[test]
fn client_key_ipv6_prefix_groups_a_subnet() {
    let k = |s: &str, p| ClientKey::new(ip(s), p);
    let a = k("2001:db8:1:2:aaaa::1", 64);
    let b = k("2001:db8:1:2:bbbb::7", 64);
    let c = k("2001:db8:1:3::1", 64);
    assert_eq!(a.as_bytes().len(), 16);
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_eq!(
        a.as_bytes(),
        &[0x20, 0x01, 0x0d, 0xb8, 0, 1, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    // /128 keeps every bit; an odd prefix masks inside a byte.
    assert_ne!(
        k("2001:db8:1:2:aaaa::1", 128),
        k("2001:db8:1:2:bbbb::7", 128)
    );
    assert_eq!(k("2001:db8:1:2f::", 60), k("2001:db8:1:20::", 60));
    assert_ne!(k("2001:db8:1:2f::", 60), k("2001:db8:1:30::", 60));
    assert_eq!(k("2001:db8:1:2f::", 200), k("2001:db8:1:2f::", 128));
    // Every prefix length keeps exactly its bits.
    let all = Ipv6Addr::from(u128::MAX);
    for bits in 0..=128u32 {
        let got = u128::from_be_bytes(ClientKey::new(IpAddr::V6(all), bits).bytes);
        let want = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
        assert_eq!(got, want, "/{bits}");
    }
}

#[test]
fn refusal_reasons_and_counters_have_pacerd_names() {
    let reasons: Vec<_> = Refusal::ALL.iter().map(|r| r.to_string()).collect();
    assert_eq!(
        reasons,
        [
            "listener_full",
            "per_ip_conns",
            "per_ip_rate",
            "table_full",
            "fd_exhausted",
            "fd_exhausted_paused",
            "per_ip_quota",
        ]
    );
    // pd_stat_names, in order, then the quota's.
    let names: Vec<_> = Stat::ALL.iter().map(|s| s.name()).collect();
    assert_eq!(
        names,
        [
            "accepted",
            "rejected_listener_full",
            "rejected_per_ip_conns",
            "rejected_per_ip_rate",
            "rejected_table_full",
            "rejected_fd_exhausted",
            "closed_done",
            "closed_idle",
            "closed_slow",
            "closed_upstream_failed",
            "closed_reset",
            "closed_drain",
            "closed_other",
            "bytes_up",
            "bytes_down",
            "throttled",
            "rejected_per_ip_quota",
        ]
    );
    for (i, s) in Stat::ALL.iter().enumerate() {
        assert_eq!(s.index(), i);
    }
}

// proxy.c: stat_for, every row of its table and an unknown reason.
#[test]
fn reasons_map_to_counters_as_stat_for_does() {
    let map = [
        ("done", Stat::ClosedDone),
        ("idle_timeout", Stat::ClosedIdle),
        ("slow_client", Stat::ClosedSlow),
        ("connect_timeout", Stat::ClosedUpstreamFailed),
        ("upstream_refused", Stat::ClosedUpstreamFailed),
        ("upstream_unreachable", Stat::ClosedUpstreamFailed),
        ("upstream_socket", Stat::ClosedUpstreamFailed),
        ("client_reset", Stat::ClosedReset),
        ("client_error", Stat::ClosedReset),
        ("upstream_reset", Stat::ClosedReset),
        ("upstream_error", Stat::ClosedReset),
        ("drain_timeout", Stat::ClosedDrain),
        ("shutdown", Stat::ClosedDrain),
        ("listener_full", Stat::RejectedListenerFull),
        ("per_ip_conns", Stat::RejectedPerIpConns),
        ("per_ip_rate", Stat::RejectedPerIpRate),
        ("table_full", Stat::RejectedTableFull),
        ("fd_exhausted", Stat::RejectedFdExhausted),
        ("fd_exhausted_paused", Stat::RejectedFdExhausted),
        ("per_ip_quota", Stat::RejectedPerIpQuota),
        ("something_else", Stat::ClosedOther),
        ("", Stat::ClosedOther),
    ];
    let c = Counters::default();
    for (reason, stat) in map {
        assert_eq!(Stat::for_reason(reason), stat, "{reason}");
        c.count(reason);
    }
    for r in Refusal::ALL {
        assert_eq!(Stat::for_reason(r.as_str()), r.stat());
    }
    let snap = c.snapshot();
    assert_eq!(snap[0], ("accepted", 0));
    assert_eq!(c.get(Stat::ClosedUpstreamFailed), 4);
    assert_eq!(c.get(Stat::ClosedReset), 4);
    assert_eq!(c.get(Stat::RejectedFdExhausted), 2);
    assert_eq!(c.get(Stat::ClosedOther), 2);
    assert_eq!(snap.iter().map(|(_, n)| n).sum::<u64>(), map.len() as u64);
    c.add(Stat::BytesUp, 1 << 40);
    assert_eq!(c.get(Stat::BytesUp), 1 << 40);
}

// it_limits.c: limits_listener_connection_cap
#[test]
fn listener_cap_counts_across_clients() {
    let a = admission(|l| l.max_conns = 4);
    let permits: Vec<_> = (0..4)
        .map(|i| a.admit(ip(&format!("127.0.0.{}", 10 + i)), 0).unwrap())
        .collect();
    assert!(permits.iter().all(|p| !p.held()), "no per-client limit");
    assert_eq!(a.open(), 4);
    assert_eq!(
        a.admit(ip("127.0.0.20"), 0).unwrap_err(),
        Refusal::ListenerFull
    );
    assert_eq!(a.open(), 4, "a refusal takes no slot");
    // No per-client limit, so the table is never touched.
    assert_eq!(a.client_stats().inserts, 0);
    drop(permits);
    assert_eq!(a.open(), 0);
    assert!(a.admit(ip("127.0.0.20"), 0).is_ok());
    assert_eq!(a.counters().get(Stat::Accepted), 5);
    assert_eq!(a.counters().get(Stat::RejectedListenerFull), 1);
}

// it_limits.c: limits_per_ip_concurrent_connections
#[test]
fn per_ip_cap_refuses_the_fourth_and_frees_on_close() {
    let a = admission(|l| l.per_ip_max_conns = 3);
    let mut fds: Vec<_> = (0..3)
        .map(|_| a.admit(ip("127.0.0.1"), 0).unwrap())
        .collect();
    assert!(fds.iter().all(Permit::held));
    assert_eq!(
        a.admit(ip("127.0.0.1"), 0).unwrap_err(),
        Refusal::PerIpConns
    );
    // Another address is not affected.
    let other = a.admit(ip("127.0.0.2"), 0).unwrap();
    assert_eq!(other.key(), ClientKey::new(ip("127.0.0.2"), 64));
    drop(other);
    let k = ClientKey::new(ip("127.0.0.1"), 64);
    assert_eq!(a.client(&k).unwrap().holds, 3);
    // Closing one frees a slot for the first address.
    fds.remove(0);
    assert_eq!(a.client(&k).unwrap().holds, 2);
    let again = a.admit(ip("127.0.0.1"), 0).unwrap();
    assert_eq!(a.open(), 3);
    drop(again);
    drop(fds);
    assert_eq!(a.client(&k).unwrap().holds, 0);
    assert_eq!(a.client_stats().holds, 0);
    assert_eq!(a.open(), 0);
    let c = a.counters();
    assert_eq!(
        (c.get(Stat::Accepted), c.get(Stat::RejectedPerIpConns)),
        (5, 1)
    );
}

// it_limits.c: limits_per_ip_slot_is_free_once_close_is_logged. The slot
// is back before drop returns, so an immediate reconnect is admitted.
#[test]
fn per_ip_slot_is_free_as_soon_as_the_permit_drops() {
    let a = admission(|l| l.per_ip_max_conns = 1);
    for i in 0..200 {
        let p = a.admit(ip("127.0.0.1"), i * MS).unwrap();
        assert_eq!(
            a.admit(ip("127.0.0.1"), i * MS).unwrap_err(),
            Refusal::PerIpConns
        );
        drop(p);
    }
    assert_eq!(a.counters().get(Stat::Accepted), 200);
}

// it_limits.c: limits_per_ip_connection_rate_recovers
#[test]
fn per_ip_rate_refuses_then_recovers() {
    let a = admission(|l| l.per_ip_conn_rate = Some(Rate::new(5, SEC, 5)));
    for _ in 0..5 {
        let p = a.admit(ip("127.0.0.1"), 0).unwrap();
        assert!(
            p.held(),
            "a rate alone still holds, so a reload can add a cap"
        );
    }
    assert_eq!(a.admit(ip("127.0.0.1"), 0).unwrap_err(), Refusal::PerIpRate);
    // A refusal for the rate takes no hold and no token.
    let k = ClientKey::new(ip("127.0.0.1"), 64);
    assert_eq!(a.client(&k).unwrap().holds, 0);
    // One token returns every 200 ms.
    assert_eq!(
        a.admit(ip("127.0.0.1"), 200 * MS - 1).unwrap_err(),
        Refusal::PerIpRate
    );
    assert!(a.admit(ip("127.0.0.1"), 200 * MS).is_ok());
    assert_eq!(
        a.admit(ip("127.0.0.1"), 200 * MS).unwrap_err(),
        Refusal::PerIpRate
    );
    assert!(a.admit(ip("127.0.0.2"), 0).is_ok(), "per client");
    assert_eq!(a.counters().get(Stat::RejectedPerIpRate), 3);
}

#[test]
fn a_cap_refusal_takes_no_token() {
    let a = admission(|l| {
        l.per_ip_max_conns = 1;
        l.per_ip_conn_rate = Some(Rate::new(2, SEC, 2));
    });
    let p = a.admit(ip("10.0.0.1"), 0).unwrap();
    for _ in 0..10 {
        assert_eq!(a.admit(ip("10.0.0.1"), 0).unwrap_err(), Refusal::PerIpConns);
    }
    drop(p);
    // The second token of the burst is still there.
    let p = a.admit(ip("10.0.0.1"), 0).unwrap();
    drop(p);
    assert_eq!(a.admit(ip("10.0.0.1"), 0).unwrap_err(), Refusal::PerIpRate);
}

#[test]
fn listener_cap_is_checked_before_the_client() {
    let a = admission(|l| {
        l.max_conns = 1;
        l.per_ip_max_conns = 5;
        l.per_ip_conn_rate = Some(Rate::new(1, SEC, 1));
    });
    let _p = a.admit(ip("10.0.0.1"), 0).unwrap();
    assert_eq!(
        a.admit(ip("10.0.0.2"), 0).unwrap_err(),
        Refusal::ListenerFull
    );
    // The refused client was never put in the table.
    assert!(a.client(&ClientKey::new(ip("10.0.0.2"), 64)).is_none());
    assert_eq!(a.client_stats().entries, 1);
}

#[test]
fn ipv6_clients_share_limits_by_prefix() {
    let a = admission(|l| {
        l.per_ip_max_conns = 2;
        l.ipv6_prefix = 56;
    });
    let _x = a.admit(ip("2001:db8:0:1::1"), 0).unwrap();
    let _y = a.admit(ip("2001:db8:0:ff::2"), 0).unwrap();
    assert_eq!(
        a.admit(ip("2001:db8:0:aa::3"), 0).unwrap_err(),
        Refusal::PerIpConns
    );
    let _z = a.admit(ip("2001:db8:0:100::1"), 0).unwrap();
    // An IPv4 client over IPv6 is counted with its IPv4 address.
    let _v4 = a.admit(ip("192.0.2.1"), 0).unwrap();
    let _v4b = a.admit(ip("::ffff:192.0.2.1"), 0).unwrap();
    assert_eq!(
        a.admit(ip("192.0.2.1"), 0).unwrap_err(),
        Refusal::PerIpConns
    );
}

#[test]
fn a_full_table_refuses_new_clients_and_keeps_existing_limits() {
    // One shard of four keys, deny policy.
    let a = admission(|l| {
        l.table_size = 4;
        l.per_ip_max_conns = 1;
    });
    let held: Vec<_> = (1..=4)
        .map(|i| a.admit(ip(&format!("10.0.0.{i}")), 0).unwrap())
        .collect();
    assert_eq!(a.admit(ip("10.0.0.5"), 0).unwrap_err(), Refusal::TableFull);
    assert_eq!(a.open(), 4, "a table refusal gives the slot back");
    // Existing clients keep their limits.
    assert_eq!(a.admit(ip("10.0.0.1"), 0).unwrap_err(), Refusal::PerIpConns);
    assert_eq!(a.client_stats().rejections, 1);
    // Once a client has closed and its bucket refilled, its entry is
    // idle and can be reclaimed.
    drop(held);
    assert!(a.admit(ip("10.0.0.5"), SEC).is_ok());
    assert_eq!(a.client_stats().reclaims, 1);
    assert_eq!(a.counters().get(Stat::RejectedTableFull), 1);
}

#[test]
fn evict_never_drops_a_client_with_open_connections() {
    let a = admission(|l| {
        l.table_size = 4;
        l.table_full = FullPolicy::Evict;
        l.per_ip_max_conns = 1;
    });
    let held: Vec<_> = (1..=4)
        .map(|i| a.admit(ip(&format!("10.0.0.{i}")), 0).unwrap())
        .collect();
    // Every entry holds a connection, so evict has nothing to drop.
    assert_eq!(a.admit(ip("10.0.0.5"), 0).unwrap_err(), Refusal::TableFull);
    drop(held);
    // With debt but no holds, the coldest is evicted.
    let a = admission(|l| {
        l.table_size = 4;
        l.table_full = FullPolicy::Evict;
        l.per_ip_conn_rate = Some(Rate::new(1, 3600 * SEC, 1));
    });
    for i in 1..=4 {
        drop(a.admit(ip(&format!("10.0.0.{i}")), 0).unwrap());
    }
    assert!(a.admit(ip("10.0.0.5"), 0).is_ok());
    assert_eq!(a.client_stats().evictions, 1);
    // The evicted client lost its debt with its entry.
    assert!(a.admit(ip("10.0.0.1"), 0).is_ok());
}

#[test]
fn reconfigure_changes_limits_in_place() {
    let mut l = listener();
    l.max_conns = 2;
    l.per_ip_max_conns = 1;
    let a = Arc::new(Admission::new(&l).unwrap());
    let p1 = a.admit(ip("10.0.0.1"), 0).unwrap();
    assert_eq!(a.admit(ip("10.0.0.1"), 0).unwrap_err(), Refusal::PerIpConns);

    // Raise the client cap and add a rate: the open hold is kept, and the
    // debt left by the unlimited rate is kept as time.
    l.per_ip_max_conns = 3;
    l.per_ip_conn_rate = Some(Rate::new(1, SEC, 1));
    a.reconfigure(&l).unwrap();
    assert_eq!(a.admit(ip("10.0.0.1"), 0).unwrap_err(), Refusal::PerIpRate);
    let p2 = a.admit(ip("10.0.0.1"), SEC).unwrap();
    assert_eq!(
        a.admit(ip("10.0.0.1"), SEC).unwrap_err(),
        Refusal::ListenerFull
    );
    let k = ClientKey::new(ip("10.0.0.1"), 64);
    assert_eq!(a.client(&k).unwrap().holds, 2);

    // Turn the per-client limits off: no table call, but the open holds
    // are still released on close.
    l.max_conns = 10;
    l.per_ip_max_conns = 0;
    l.per_ip_conn_rate = None;
    a.reconfigure(&l).unwrap();
    let p3 = a.admit(ip("10.0.0.1"), 0).unwrap();
    assert!(!p3.held());
    drop((p1, p2, p3));
    assert_eq!(a.client(&k).unwrap().holds, 0);
    assert_eq!(a.open(), 0);

    // Lowering max_conns below the open count refuses until enough close.
    let open: Vec<_> = (0..5)
        .map(|_| a.admit(ip("10.0.0.9"), 0).unwrap())
        .collect();
    l.max_conns = 3;
    a.reconfigure(&l).unwrap();
    assert_eq!(
        a.admit(ip("10.0.0.8"), 0).unwrap_err(),
        Refusal::ListenerFull
    );
    drop(open);
    assert!(a.admit(ip("10.0.0.8"), 0).is_ok());

    // The prefix changes with a reload too.
    l.ipv6_prefix = 128;
    l.per_ip_max_conns = 1;
    a.reconfigure(&l).unwrap();
    let _x = a.admit(ip("2001:db8::1"), 0).unwrap();
    assert!(a.admit(ip("2001:db8::2"), 0).is_ok());
}

#[test]
fn reconfigure_refuses_what_needs_a_restart() {
    let l = listener();
    let a = Admission::new(&l).unwrap();
    let mut m = l.clone();
    m.table_size = 100;
    m.max_conns = 1;
    assert_eq!(
        a.reconfigure(&m).unwrap_err(),
        "listener web: table_size cannot change without a restart"
    );
    let mut m = l.clone();
    m.table_full = FullPolicy::Evict;
    assert_eq!(
        a.reconfigure(&m).unwrap_err(),
        "listener web: table_full cannot change without a restart"
    );
    let mut m = l.clone();
    m.per_ip_conn_rate = Some(Rate::new(0, SEC, 1));
    assert_eq!(
        a.reconfigure(&m).unwrap_err(),
        "listener web: per_ip_conn_rate: invalid argument"
    );
    // Nothing changed.
    assert_eq!(a.caps(), (1024, 0));
    assert_eq!(a.name(), "web");
}

#[test]
fn new_refuses_what_the_table_refuses() {
    let mut l = listener();
    l.table_size = 0;
    assert_eq!(Admission::new(&l).unwrap_err(), Error::InvalidConfig);
    let mut l = listener();
    l.per_ip_conn_rate = Some(Rate::new(1, SEC, 0));
    assert_eq!(Admission::new(&l).unwrap_err(), Error::InvalidRate);
}

#[test]
fn memory_is_fixed_by_the_table_size() {
    let a = admission(|l| l.table_size = 16_384);
    let m = a.memory();
    assert!(m >= 16_384 * 64, "{m}");
    for i in 0..20_000u32 {
        let _ = a.admit(IpAddr::V4(Ipv4Addr::from(i)), u64::from(i));
    }
    assert_eq!(a.memory(), m);
    assert_eq!(a.client_stats().capacity, 16_384);
}

#[test]
fn refuse_counts_refusals_made_elsewhere_and_sweep_drops_idle_clients() {
    let a = admission(|l| l.per_ip_conn_rate = Some(Rate::new(1, SEC, 1)));
    a.refuse(Refusal::FdExhausted);
    a.refuse(Refusal::FdExhaustedPaused);
    assert_eq!(a.counters().get(Stat::RejectedFdExhausted), 2);
    for i in 0..10u32 {
        drop(a.admit(IpAddr::V4(Ipv4Addr::from(i)), 0).unwrap());
    }
    assert_eq!(a.client_stats().entries, 10);
    assert_eq!(a.sweep(0, u32::MAX), 0, "every client still owes");
    assert_eq!(a.sweep(SEC, u32::MAX), 10);
    assert_eq!(a.client_stats().entries, 0);
}

#[test]
fn debug_output_names_the_listener() {
    let a = admission(|l| l.per_ip_max_conns = 2);
    let p = a.admit(ip("10.1.2.3"), 0).unwrap();
    assert_eq!(
        format!("{p:?}"),
        "Permit { listener: \"web\", key: ClientKey(0a010203), held: true }"
    );
    let d = format!("{a:?}");
    assert!(
        d.starts_with("Admission { name: \"web\", max_conns: 1024, per_ip_max_conns: 2, open: 1,"),
        "{d}"
    );
    assert!(Arc::ptr_eq(p.admission(), &a));
}

/// Threads racing to admit never exceed either cap, and every refusal
/// gives back what it took.
#[test]
fn racing_accepts_never_exceed_the_caps() {
    const THREADS: usize = 8;
    const ROUNDS: usize = 2_000;
    let a = admission(|l| {
        l.max_conns = 12;
        l.per_ip_max_conns = 3;
    });
    let clients = ["10.0.0.1", "10.0.0.2", "10.0.0.3", "10.0.0.4", "10.0.0.5"].map(ip);
    let per_client = clients.map(|_| AtomicUsize::new(0));
    let total = AtomicUsize::new(0);
    let over = Mutex::new(Vec::new());
    let start = Barrier::new(THREADS);
    thread::scope(|s| {
        for t in 0..THREADS {
            let (a, clients, per_client, total, over, start) =
                (&a, &clients, &per_client, &total, &over, &start);
            s.spawn(move || {
                start.wait();
                let mut mine = Vec::new();
                for r in 0..ROUNDS {
                    let c = (t * 7 + r) % clients.len();
                    if let Ok(p) = a.admit(clients[c], 0) {
                        let n = per_client[c].fetch_add(1, Ordering::SeqCst) + 1;
                        let all = total.fetch_add(1, Ordering::SeqCst) + 1;
                        if n > 3 || all > 12 {
                            over.lock().unwrap().push((c, n, all));
                        }
                        mine.push((c, p));
                    }
                    if mine.len() > 2 || (r % 3 == 0 && !mine.is_empty()) {
                        let (c, p) = mine.remove(0);
                        per_client[c].fetch_sub(1, Ordering::SeqCst);
                        total.fetch_sub(1, Ordering::SeqCst);
                        drop(p);
                    }
                }
                for (c, p) in mine {
                    per_client[c].fetch_sub(1, Ordering::SeqCst);
                    total.fetch_sub(1, Ordering::SeqCst);
                    drop(p);
                }
            });
        }
    });
    assert_eq!(*over.lock().unwrap(), []);
    assert_eq!(a.open(), 0);
    assert_eq!(a.client_stats().holds, 0);
    let c = a.counters();
    let decided: u64 = [
        Stat::Accepted,
        Stat::RejectedListenerFull,
        Stat::RejectedPerIpConns,
    ]
    .iter()
    .map(|&s| c.get(s))
    .sum();
    assert_eq!(decided, (THREADS * ROUNDS) as u64);
    assert!(c.get(Stat::Accepted) > 0);
}

const HOUR: u64 = 3_600 * SEC;
const DAY: u64 = 24 * HOUR;

fn quota(limit: u64, window_ns: u64) -> Option<ConnQuota> {
    Some(ConnQuota {
        limit,
        window: Duration::from_nanos(window_ns),
    })
}

#[test]
fn a_quota_refuses_past_its_limit_at_the_window_edge() {
    // No cap and no rate: only the quota table is used.
    let a = admission(|l| l.per_ip_conn_quota = quota(3, HOUR));
    let k = ClientKey::new(ip("10.0.0.1"), 64);
    let open: Vec<_> = (0..3)
        .map(|_| a.admit(ip("10.0.0.1"), 100 * SEC).unwrap())
        .collect();
    assert!(open.iter().all(|p| !p.held()));
    drop(open);
    // Closing does not give a quota back: it counts new connections.
    assert_eq!(
        a.admit(ip("10.0.0.1"), 100 * SEC).unwrap_err(),
        Refusal::PerIpQuota
    );
    assert_eq!(a.open(), 0, "a quota refusal gives the slot back");
    assert_eq!(a.client_stats().inserts, 0, "the client table is not used");
    assert_eq!(a.quota_count(&k, 100 * SEC), Some(3));
    // The three are in the window until an hour after their bucket
    // started; just past that, the oldest bucket is weighted down.
    assert_eq!(
        a.admit(ip("10.0.0.1"), HOUR).unwrap_err(),
        Refusal::PerIpQuota
    );
    assert!(a.admit(ip("10.0.0.1"), HOUR + 1).is_ok());
    assert_eq!(
        a.admit(ip("10.0.0.1"), HOUR + 1).unwrap_err(),
        Refusal::PerIpQuota
    );
    // Another client, and an IPv6 client by its prefix, have their own.
    assert!(a.admit(ip("10.0.0.2"), 0).is_ok());
    assert!(a.admit(ip("2001:db8::1"), 0).is_ok());
    assert_eq!(
        a.quota_count(&ClientKey::new(ip("2001:db8::2"), 64), 0),
        Some(1)
    );
    let c = a.counters();
    assert_eq!(
        (c.get(Stat::Accepted), c.get(Stat::RejectedPerIpQuota)),
        (6, 3)
    );
    assert_eq!(a.quota_stats().unwrap().entries, 3);
}

#[test]
fn a_quota_refusal_gives_back_the_hold_and_the_token() {
    let a = admission(|l| {
        l.per_ip_max_conns = 5;
        l.per_ip_conn_rate = Some(Rate::new(2, SEC, 2));
        l.per_ip_conn_quota = quota(1, HOUR);
    });
    let k = ClientKey::new(ip("10.0.0.1"), 64);
    let p = a.admit(ip("10.0.0.1"), 0).unwrap();
    assert!(p.held());
    let before = a.client(&k).unwrap();
    assert_eq!(before.holds, 1);
    for _ in 0..5 {
        assert_eq!(a.admit(ip("10.0.0.1"), 0).unwrap_err(), Refusal::PerIpQuota);
    }
    // Nothing was taken: the same holds and the same arrival time, so the
    // second token of the burst is still there.
    assert_eq!(a.client(&k).unwrap(), before);
    assert_eq!(a.open(), 1);
    assert_eq!(a.client_stats().holds, 1);
    a.reconfigure(&{
        let mut l = listener();
        l.per_ip_max_conns = 5;
        l.per_ip_conn_rate = Some(Rate::new(2, SEC, 2));
        l.per_ip_conn_quota = quota(3, HOUR);
        l
    })
    .unwrap();
    let q = a.admit(ip("10.0.0.1"), 0).unwrap();
    // Two tokens are gone now, so the rate refuses before the quota.
    assert_eq!(a.admit(ip("10.0.0.1"), 0).unwrap_err(), Refusal::PerIpRate);
    drop((p, q));
    assert_eq!(a.client(&k).unwrap().holds, 0);
    assert_eq!(a.open(), 0);
    // The cap and the rate are checked first, so their refusals leave the
    // quota alone.
    assert_eq!(a.quota_count(&k, 0), Some(2));
    let c = a.counters();
    assert_eq!(
        (
            c.get(Stat::Accepted),
            c.get(Stat::RejectedPerIpQuota),
            c.get(Stat::RejectedPerIpRate)
        ),
        (2, 5, 1)
    );
}

#[test]
fn a_full_quota_table_refuses_new_clients_as_table_full() {
    let a = admission(|l| {
        l.table_size = 4;
        l.per_ip_max_conns = 1;
        l.per_ip_conn_quota = quota(10, DAY);
    });
    for i in 1..=4 {
        drop(a.admit(ip(&format!("10.0.0.{i}")), 0).unwrap());
    }
    // A second on, the client table reclaims the idle client 1 for client
    // 5, but the quota table still counts all four in their window.
    assert_eq!(
        a.admit(ip("10.0.0.5"), SEC).unwrap_err(),
        Refusal::TableFull
    );
    assert_eq!(a.client_stats().reclaims, 1);
    assert_eq!(a.open(), 0);
    assert_eq!(a.client_stats().holds, 0);
    assert_eq!(a.quota_stats().unwrap().rejections, 1);
    assert_eq!(a.counters().get(Stat::RejectedTableFull), 1);
    // Existing clients carry on.
    assert!(a.admit(ip("10.0.0.2"), SEC).is_ok());
    // Once their windows are empty they can be reclaimed.
    assert!(a.admit(ip("10.0.0.5"), 2 * DAY).is_ok());
    assert_eq!(a.quota_stats().unwrap().reclaims, 1);

    // Under evict, the coldest client's count goes to make room.
    let a = admission(|l| {
        l.table_size = 4;
        l.table_full = FullPolicy::Evict;
        l.per_ip_conn_quota = quota(1, DAY);
    });
    for i in 1..=4 {
        assert!(a.admit(ip(&format!("10.0.0.{i}")), 0).is_ok());
    }
    assert!(a.admit(ip("10.0.0.5"), 0).is_ok());
    assert_eq!(a.quota_stats().unwrap().evictions, 1);
    assert!(a.admit(ip("10.0.0.1"), 0).is_ok(), "evicted with its count");
    assert_eq!(a.admit(ip("10.0.0.5"), 0).unwrap_err(), Refusal::PerIpQuota);
}

#[test]
fn reconfigure_changes_a_quota_in_place() {
    let mut l = listener();
    l.per_ip_conn_quota = quota(2, HOUR);
    let a = Arc::new(Admission::new(&l).unwrap());
    let k = ClientKey::new(ip("10.0.0.1"), 64);
    for _ in 0..2 {
        assert!(a.admit(ip("10.0.0.1"), 0).is_ok());
    }
    assert_eq!(a.admit(ip("10.0.0.1"), 0).unwrap_err(), Refusal::PerIpQuota);

    // A new limit keeps the count.
    l.per_ip_conn_quota = quota(3, HOUR);
    a.reconfigure(&l).unwrap();
    assert!(a.admit(ip("10.0.0.1"), 0).is_ok());
    assert_eq!(a.admit(ip("10.0.0.1"), 0).unwrap_err(), Refusal::PerIpQuota);
    assert_eq!(a.quota_count(&k, 0), Some(3));

    // A new window starts everyone afresh, with day-long windows.
    l.per_ip_conn_quota = quota(1, DAY);
    a.reconfigure(&l).unwrap();
    assert_eq!(a.quota_count(&k, 0), None);
    assert!(a.admit(ip("10.0.0.1"), 0).is_ok());
    assert_eq!(
        a.admit(ip("10.0.0.1"), 2 * HOUR).unwrap_err(),
        Refusal::PerIpQuota
    );

    // Switched off, nobody is refused and nothing is counted.
    l.per_ip_conn_quota = None;
    a.reconfigure(&l).unwrap();
    for _ in 0..5 {
        assert!(a.admit(ip("10.0.0.1"), 2 * HOUR).is_ok());
    }
    assert_eq!(a.quota_stats().unwrap().entries, 0);
    // Reloading with it still off changes nothing.
    a.reconfigure(&l).unwrap();
    // Back on, with the same window, counts start from zero.
    l.per_ip_conn_quota = quota(1, DAY);
    a.reconfigure(&l).unwrap();
    assert!(a.admit(ip("10.0.0.1"), 2 * HOUR).is_ok());
    assert_eq!(
        a.admit(ip("10.0.0.1"), 2 * HOUR).unwrap_err(),
        Refusal::PerIpQuota
    );
    assert_eq!(a.counters().get(Stat::RejectedPerIpQuota), 4);
}

#[test]
fn reconfigure_refuses_a_quota_it_cannot_apply() {
    // Started without a quota, there is no table to count in.
    let l = listener();
    let a = Admission::new(&l).unwrap();
    assert_eq!(a.quota_stats(), None);
    assert_eq!(a.quota_count(&ClientKey::new(ip("10.0.0.1"), 64), 0), None);
    let mut m = l.clone();
    m.per_ip_conn_quota = quota(5, DAY);
    m.max_conns = 1;
    assert_eq!(
        a.reconfigure(&m).unwrap_err(),
        "listener web: per_ip_conn_quota cannot be switched on without a restart"
    );
    assert_eq!(a.caps(), (1024, 0), "nothing changed");

    // An invalid quota or rate leaves the quota as it was.
    let mut l = listener();
    l.per_ip_conn_quota = quota(1, HOUR);
    let a = Arc::new(Admission::new(&l).unwrap());
    assert!(a.admit(ip("10.0.0.1"), 0).is_ok());
    let mut m = l.clone();
    m.per_ip_conn_quota = quota(0, DAY);
    assert_eq!(
        a.reconfigure(&m).unwrap_err(),
        "listener web: per_ip_conn_quota: invalid argument"
    );
    m.per_ip_conn_quota = Some(ConnQuota {
        limit: 1,
        window: Duration::from_secs(u64::MAX),
    });
    assert_eq!(
        a.reconfigure(&m).unwrap_err(),
        "listener web: per_ip_conn_quota: invalid window configuration"
    );
    m.per_ip_conn_quota = quota(5, DAY);
    m.per_ip_conn_rate = Some(Rate::new(0, SEC, 1));
    assert!(a.reconfigure(&m).is_err());
    assert_eq!(a.admit(ip("10.0.0.1"), 0).unwrap_err(), Refusal::PerIpQuota);
    assert_eq!(
        a.quota_count(&ClientKey::new(ip("10.0.0.1"), 64), 0),
        Some(1)
    );

    // new() refuses the same quotas.
    let mut l = listener();
    l.per_ip_conn_quota = quota(0, HOUR);
    assert_eq!(Admission::new(&l).unwrap_err(), Error::InvalidRate);
    l.per_ip_conn_quota = quota(1, 15);
    assert_eq!(Admission::new(&l).unwrap_err(), Error::InvalidWindow);
}

#[test]
fn a_quota_table_is_fixed_memory_and_swept_with_the_clients() {
    let a = admission(|l| {
        l.table_size = 16_384;
        l.per_ip_conn_rate = Some(Rate::new(1, SEC, 1));
        l.per_ip_conn_quota = quota(5, HOUR);
    });
    let without = admission(|l| {
        l.table_size = 16_384;
        l.per_ip_conn_rate = Some(Rate::new(1, SEC, 1));
    });
    let m = a.memory();
    assert!(m > without.memory() + 16_384 * 64, "{m}");
    for i in 0..20_000u32 {
        let _ = a.admit(IpAddr::V4(Ipv4Addr::from(i)), u64::from(i));
    }
    assert_eq!(a.memory(), m);
    assert_eq!(a.quota_stats().unwrap().capacity, 16_384);
    let (clients, windows) = (a.client_stats().entries, a.quota_stats().unwrap().entries);
    assert!(windows > 15_000, "{windows}");
    // A second on, every client's rate has refilled but every window
    // still counts.
    assert_eq!(a.sweep(2 * SEC, u32::MAX) as u64, clients);
    assert_eq!(a.quota_stats().unwrap().entries, windows);
    assert_eq!(a.sweep(2 * HOUR, u32::MAX) as u64, windows);
    assert_eq!(a.quota_stats().unwrap().entries, 0);
}

/// Threads racing to admit one client never get more than its quota, and
/// every refusal gives back what it took.
#[test]
fn racing_accepts_never_exceed_the_quota() {
    const THREADS: usize = 8;
    const ROUNDS: usize = 1_000;
    let a = admission(|l| {
        l.per_ip_max_conns = 4;
        l.per_ip_conn_quota = quota(500, HOUR);
    });
    let admitted = AtomicUsize::new(0);
    let start = Barrier::new(THREADS);
    thread::scope(|s| {
        for _ in 0..THREADS {
            let (a, admitted, start) = (&a, &admitted, &start);
            s.spawn(move || {
                start.wait();
                for _ in 0..ROUNDS {
                    if let Ok(p) = a.admit(ip("10.0.0.1"), 0) {
                        admitted.fetch_add(1, Ordering::Relaxed);
                        drop(p);
                    }
                }
            });
        }
    });
    assert_eq!(admitted.load(Ordering::Relaxed), 500);
    assert_eq!(a.open(), 0);
    assert_eq!(a.client_stats().holds, 0);
    let c = a.counters();
    assert_eq!(c.get(Stat::Accepted), 500);
    assert_eq!(
        c.get(Stat::RejectedPerIpQuota) + c.get(Stat::RejectedPerIpConns),
        (THREADS * ROUNDS - 500) as u64
    );
}

#[test]
fn counters_added_from_many_threads_sum_across_their_stripes() {
    let c = Arc::new(Counters::default());
    let hs: Vec<_> = (0..40)
        .map(|i| {
            let c = Arc::clone(&c);
            thread::spawn(move || {
                for _ in 0..1000 {
                    c.add(Stat::BytesUp, i + 1);
                }
                c.count("done");
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    // 1000 * (1 + 2 + ... + 40)
    assert_eq!(c.get(Stat::BytesUp), 820_000);
    assert_eq!(c.get(Stat::ClosedDone), 40);
    assert_eq!(std::mem::align_of::<Stripe>(), 128);
    // Counters wrap at 2^64 as one sum, as pacerd's did.
    let w = Counters::default();
    w.add(Stat::BytesDown, u64::MAX);
    thread::spawn({
        let w = Arc::new(w);
        move || {
            w.add(Stat::BytesDown, 2);
            assert_eq!(w.get(Stat::BytesDown), 1);
        }
    })
    .join()
    .unwrap();
}
