use super::*;
use crate::config::{parse_pacerd, parse_pacerd_mapped};
use crate::gcra::Gcra;
use crate::table::{FullPolicy, Table, TableConfig};
use crate::{Error, KeyHash};

/// Lints a pacerd.conf, which must parse.
fn lints(text: &str) -> Vec<Lint> {
    let (cfg, map) = parse_pacerd_mapped(text.as_bytes()).expect("a valid config");
    lint(&cfg, &map)
}

/// `(line, class)` of each finding.
fn found(text: &str) -> Vec<(usize, Class)> {
    lints(text).iter().map(|l| (l.line, l.class)).collect()
}

/// One listener with `body` inside its block, which opens on line 1.
fn one(body: &str) -> String {
    format!("listener web {{\nlisten 0.0.0.0:8080\nupstream 127.0.0.1:9000\n{body}}}\n")
}

#[test]
fn class_names_are_unique_and_read_back() {
    for (i, c) in Class::ALL.iter().enumerate() {
        assert_eq!(Class::from_name(c.as_str()), Some(*c));
        assert_eq!(c.to_string(), c.as_str());
        assert!(
            c.as_str()
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        );
        for d in &Class::ALL[i + 1..] {
            assert_ne!(c.as_str(), d.as_str());
        }
    }
    assert_eq!(Class::from_name("nope"), None);
    assert!(Class::ALL.len() >= 8);
}

#[test]
fn every_class_is_in_the_module_docs() {
    let src = include_str!("../lint.rs");
    for c in Class::ALL {
        assert!(src.contains(&format!("//! | `{c}` |")), "{c}");
    }
}

#[test]
fn pacers_example_and_a_bare_listener_are_clean() {
    let example = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../legacy/pacer/examples/pacerd.conf"
    );
    let text = std::fs::read_to_string(example).unwrap();
    assert_eq!(lints(&text), []);
    assert_eq!(lints(&one("")), []);
}

#[test]
fn the_source_map_names_every_directive_line() {
    let text = "workers 2\n\nlog_level warn\nlistener a {\n  listen 0.0.0.0:1\n  # x\n  upstream 127.0.0.1:2\n  table_size 64\n}\nlistener b {\nlisten 0.0.0.0:3\nupstream 127.0.0.1:4\n}\n";
    let (cfg, map) = parse_pacerd_mapped(text.as_bytes()).unwrap();
    assert_eq!(cfg, parse_pacerd(text.as_bytes()).unwrap());
    assert_eq!(map.top.get("workers"), Some(1));
    assert_eq!(map.top.get("log_level"), Some(3));
    assert_eq!(map.top.get("drain_timeout"), None);
    assert_eq!(map.listeners.len(), 2);
    let a = &map.listeners[0];
    assert_eq!(a.line, 4);
    assert_eq!(a.line_of("listen"), 5);
    assert_eq!(a.line_of("upstream"), 7);
    assert_eq!(a.line_of("table_size"), 8);
    assert_eq!(a.line_of("max_conns"), 4, "a default names the block");
    assert_eq!(map.listeners[1].line, 10);
    assert_eq!(map.listeners[1].line_of("upstream"), 12);
    // Listener directives never land in the top scope.
    assert_eq!(map.top.get("listen"), None);
}

#[test]
fn lines_ignore_unknown_names_and_keep_the_last_line() {
    let mut l = ListenerLines::new(3);
    l.set("name", 4);
    l.set("bogus", 5);
    assert_eq!(l.directives, Default::default());
    l.set("listen", 6);
    l.set("listen", 7);
    assert_eq!(l.line_of("listen"), 7);
    assert_eq!(l.line_of("bogus"), 3);
    let names: Vec<_> = crate::config::directive_names().collect();
    assert_eq!(names.len(), 18);
    assert_eq!(names[0], "workers");
    assert!(names.contains(&"table_full"));
}

#[test]
fn burst_below_read_names_the_cut_and_the_wakeups() {
    let text = one("bandwidth 1m/s burst 1k\nper_ip_bandwidth 64k/s burst 4k\n");
    let got = lints(&text);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!((got[0].line, got[0].class), (4, Class::BurstBelowRead));
    assert_eq!(
        got[0].to_string(),
        "warning[burst-below-read]: listener web: bandwidth 1048576/s burst 1024: a burst of 1024 bytes is below the 4096-byte read, so each shaped read moves at most 1024 bytes and a busy connection wakes about 1024 times a second"
    );
    // A buffer below 4 KiB is the read: a burst of the whole buffer is fine.
    assert_eq!(found(&one("buffer 2k\nbandwidth 1m/s burst 2k\n")), []);
    assert_eq!(
        found(&one("buffer 2k\nper_ip_bandwidth 1m/s burst 2047\n")),
        [(5, Class::BurstBelowRead)]
    );
}

#[test]
fn a_burst_below_the_read_cuts_reads_rather_than_refusing_them() {
    // What proxy.c's reserve does with buffer 16k and a 1k burst: ask for
    // 4 KiB to 16 KiB. libpacer clamps the minimum to the burst.
    let g = Gcra::new(&Rate::new(1 << 20, 1_000_000_000, 1024)).unwrap();
    let mut now = 1_000_000_000;
    for i in 0..100 {
        let d = g.take_range(now, READ_QUANTUM, 16 << 10);
        assert!(d.allowed, "read {i}: {d:?}");
        assert_eq!(d.granted, 1024);
        // Refused until the whole burst is back, then granted again.
        let early = g.take_range(now + d.retry_after_ns, READ_QUANTUM, 16 << 10);
        assert!(!early.allowed, "read {i}: {early:?}");
        now += early.retry_after_ns;
    }
}

#[test]
fn listen_overlap_is_reported_on_the_later_listener_either_way() {
    let text = "listener all {\nlisten 0.0.0.0:80\nupstream 10.0.0.9:80\n}\nlistener one {\nlisten 10.0.0.1:80\nupstream 10.0.0.9:81\n}\nlistener v6 {\nlisten [::]:80\nupstream 10.0.0.9:82\n}\nlistener v6one {\nlisten [::1]:80\nupstream 10.0.0.9:83\n}\nlistener other {\nlisten 10.0.0.2:81\nupstream 10.0.0.9:84\n}\n";
    let got = lints(text);
    assert_eq!(
        got.iter().map(|l| (l.line, l.class)).collect::<Vec<_>>(),
        [(6, Class::ListenOverlap), (14, Class::ListenOverlap)]
    );
    assert_eq!(
        got[0].message,
        "10.0.0.1:80 overlaps listener all's 0.0.0.0:80: connections to 10.0.0.1:80 go to listener one, so listener all's limits never apply to them"
    );
    assert_eq!(got[1].listener, "v6one");
    // The wildcard after the specific address.
    let text = "listener one {\nlisten 10.0.0.1:80\nupstream 10.0.0.9:1\n}\nlistener all {\nlisten 0.0.0.0:80\nupstream 10.0.0.9:2\n}\n";
    let got = lints(text);
    assert_eq!((got[0].line, got[0].class), (6, Class::ListenOverlap));
    assert_eq!(
        got[0].message,
        "0.0.0.0:80 overlaps listener one's 10.0.0.1:80: connections to 10.0.0.1:80 go to listener one, so listener all's limits never apply to them"
    );
    // IPv4 and IPv6 wildcards do not overlap: IPv6 sockets are V6ONLY.
    let text = "listener a {\nlisten 0.0.0.0:80\nupstream 10.0.0.9:1\n}\nlistener b {\nlisten [::]:80\nupstream 10.0.0.9:2\n}\n";
    assert_eq!(found(text), []);
}

#[test]
fn table_below_max_conns_needs_per_client_admission() {
    let text = one("max_conns 100\ntable_size 64\nper_ip_max_conns 4\n");
    let got = lints(&text);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!((got[0].line, got[0].class), (5, Class::TableBelowMaxConns));
    assert!(
        got[0]
            .message
            .starts_with("table_size 64 is below max_conns 100")
    );
    // The default table size, on the block's line.
    assert_eq!(
        found(&one("max_conns 20000\nper_ip_conn_rate 10/s\n")),
        [(1, Class::TableBelowMaxConns)]
    );
    // Bandwidth entries are never held, so they do not count.
    assert_eq!(
        found(&one(
            "max_conns 100\ntable_size 64\nper_ip_bandwidth 1m/s\n"
        )),
        []
    );
    assert_eq!(
        found(&one("max_conns 64\ntable_size 64\nper_ip_max_conns 4\n")),
        []
    );
}

#[test]
fn a_table_of_held_clients_refuses_new_ones_under_either_policy() {
    for policy in [FullPolicy::Deny, FullPolicy::Evict] {
        let t = Table::new(&TableConfig {
            max_entries: 16,
            shards: 1,
            rate: Rate::new(1_000_000_000, 1_000_000_000, 1_000_000_000),
            hash_key: Some([7; 16]),
            full_policy: policy,
            key_hash: KeyHash::Sip13,
        })
        .unwrap();
        for k in 0..16u32 {
            assert!(t.hold(&k.to_be_bytes(), 0, u32::MAX).unwrap().allowed);
        }
        assert_eq!(
            t.hold(&99u32.to_be_bytes(), 1_000_000_000_000, u32::MAX),
            Err(Error::TableFull),
            "{policy:?}"
        );
    }
}

#[test]
fn per_ip_cap_unreachable() {
    assert_eq!(
        found(&one("max_conns 10\nper_ip_max_conns 10\n")),
        [(5, Class::PerIpCapUnreachable)]
    );
    assert_eq!(found(&one("max_conns 10\nper_ip_max_conns 9\n")), []);
    // 0 is no cap at all.
    assert_eq!(found(&one("max_conns 10\nper_ip_max_conns 0\n")), []);
}

#[test]
fn per_ip_bandwidth_unreachable_compares_rates_exactly() {
    let got = lints(&one(
        "bandwidth 1m/s burst 64k\nper_ip_bandwidth 60m/m burst 64k\n",
    ));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(
        (got[0].line, got[0].class),
        (5, Class::PerIpBandwidthUnreachable)
    );
    assert_eq!(
        got[0].message,
        "per_ip_bandwidth 62914560/m burst 65536 is at least bandwidth 1048576/s burst 65536 in both rate and burst, so the listener's limit always binds first and the per-client one never does"
    );
    // One byte a minute slower, or a smaller burst, and it can bind.
    assert_eq!(
        found(&one(
            "bandwidth 1m/s burst 64k\nper_ip_bandwidth 62914559/m burst 64k\n"
        )),
        []
    );
    assert_eq!(
        found(&one(
            "bandwidth 1m/s burst 64k\nper_ip_bandwidth 2m/s burst 32k\n"
        )),
        []
    );
}

#[test]
fn rate_rounding_reports_rates_enforced_more_than_one_percent_slow() {
    // 100m/s is 0.106% slow: below the bar.
    assert_eq!(found(&one("bandwidth 100m/s burst 1m\n")), []);
    // 16g/s: 3.725 ticks round up to 4.
    let got = lints(&one("bandwidth 16g/s burst 1m\n"));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!((got[0].line, got[0].class), (4, Class::RateRounding));
    assert_eq!(
        got[0].message,
        "bandwidth 17179869184/s burst 1048576: its interval of 3.725 ticks of 1/64 ns rounds up to 4, so it is enforced at 93.1323% of the written rate (16000000000/s instead of 17179869184/s)"
    );
    // A rate above 64 billion a second has an interval under one tick.
    let got = lints(&one("per_ip_conn_rate 1048576g/s burst 1\n"));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].class, Class::RateRounding);
    assert!(
        got[0]
            .message
            .contains("its interval of 0.000 ticks of 1/64 ns rounds up to 1, the shortest"),
        "{}",
        got[0].message
    );
    assert!(got[0].message.contains("(64000000000/s instead of"));
    // The reported share is the bucket's own.
    let r = Rate::new(16 << 30, 1_000_000_000, 1 << 20);
    let (slow, of) = bucket_slowdown(&r).unwrap();
    assert!((of as f64 / (of + slow) as f64 - 0.931323).abs() < 1e-6);
}

#[test]
fn unused_table_settings_are_reported_where_written() {
    assert_eq!(
        found(&one("table_size 64\ntable_full evict\n")),
        [(4, Class::UnusedTable), (5, Class::UnusedTable)]
    );
    assert_eq!(found(&one("table_size 64\nper_ip_bandwidth 1m/s\n")), []);
    assert_eq!(
        lints(&one("table_full deny\n"))[0].message,
        "table_full has no effect: no per_ip_max_conns, per_ip_conn_rate, per_ip_conn_quota or per_ip_bandwidth uses the client table"
    );
}

#[test]
fn ipv6_prefixes_that_do_nothing_or_group_too_much() {
    let got = lints(&one("ipv6_prefix 56\nper_ip_max_conns 4\n"));
    assert_eq!(
        got.iter().map(|l| (l.line, l.class)).collect::<Vec<_>>(),
        [(4, Class::Ipv6PrefixUnused)]
    );
    assert_eq!(
        got[0].message,
        "ipv6_prefix has no effect: 0.0.0.0:8080 accepts only IPv4 clients"
    );
    let v6 =
        |body: &str| format!("listener web {{\nlisten [::]:8080\nupstream [::1]:9000\n{body}}}\n");
    assert_eq!(
        found(&v6("ipv6_prefix 56\n")),
        [(4, Class::Ipv6PrefixUnused)]
    );
    assert_eq!(
        lints(&v6("ipv6_prefix 56\n"))[0].message,
        "ipv6_prefix has no effect: no per-client limit groups clients"
    );
    assert_eq!(found(&v6("ipv6_prefix 56\nper_ip_max_conns 4\n")), []);
    assert_eq!(found(&v6("ipv6_prefix 48\nper_ip_bandwidth 1m/s\n")), []);
    let got = lints(&v6("ipv6_prefix 40\nper_ip_conn_rate 5/s\n"));
    assert_eq!(
        got.iter().map(|l| (l.line, l.class)).collect::<Vec<_>>(),
        [(4, Class::WideIpv6Prefix)]
    );
    assert!(
        got[0]
            .message
            .starts_with("ipv6_prefix 40 groups every client in a /40 as one")
    );
}

#[test]
fn upstream_loops_into_its_own_or_another_listener() {
    let text = "listener a {\nlisten 0.0.0.0:80\nupstream 127.0.0.1:80\n}\nlistener b {\nlisten 10.0.0.1:81\nupstream 10.0.0.1:81\n}\nlistener c {\nlisten [::]:82\nupstream 10.0.0.1:81\n}\nlistener d {\nlisten [::1]:83\nupstream [::]:82\n}\nlistener e {\nlisten 10.0.0.1:84\nupstream 10.0.0.7:80\n}\n";
    let got = lints(text);
    assert_eq!(
        got.iter()
            .map(|l| (l.line, l.class, l.listener.as_str()))
            .collect::<Vec<_>>(),
        [
            (3, Class::UpstreamLoop, "a"),
            (7, Class::UpstreamLoop, "b"),
            (11, Class::UpstreamLoop, "c"),
            (15, Class::UpstreamLoop, "d"),
        ]
    );
    assert_eq!(
        got[0].message,
        "upstream 127.0.0.1:80 is its own listen address, so every connection loops back into the limiter"
    );
    assert_eq!(
        got[2].message,
        "upstream 10.0.0.1:81 is served by listener b (10.0.0.1:81), so every connection loops back into the limiter"
    );
    // listener e's upstream may be another host behind a's wildcard.
}

#[test]
fn findings_sort_by_line_then_class() {
    let text = one("max_conns 20\nper_ip_max_conns 20\ntable_size 16\nbandwidth 16g/s burst 1k\n");
    assert_eq!(
        found(&text),
        [
            (5, Class::PerIpCapUnreachable),
            (6, Class::TableBelowMaxConns),
            (7, Class::BurstBelowRead),
            (7, Class::RateRounding),
        ]
    );
}

#[test]
fn a_config_without_a_map_reports_line_zero_and_skips_written_out_lints() {
    let (cfg, _) =
        parse_pacerd_mapped(one("table_size 16\nmax_conns 10\nper_ip_max_conns 10\n").as_bytes())
            .unwrap();
    let got = lint(&cfg, &SourceMap::default());
    assert_eq!(
        got.iter().map(|l| (l.line, l.class)).collect::<Vec<_>>(),
        [(0, Class::PerIpCapUnreachable)]
    );
}

#[test]
fn every_accepted_corpus_file_lints_without_panicking_on_real_lines() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../legacy/pacer");
    let mut stack = vec![std::path::PathBuf::from(root)];
    let mut accepted = 0;
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else {
                continue;
            };
            let Ok((cfg, map)) = parse_pacerd_mapped(&bytes) else {
                continue;
            };
            accepted += 1;
            let lines = bytes.split(|&b| b == b'\n').count();
            for l in lint(&cfg, &map) {
                assert!((1..=lines).contains(&l.line), "{}: {l:?}", p.display());
            }
        }
    }
    assert!(accepted >= 3, "{accepted}");
}

/// One listener from `body` in pacerd.conf syntax plus a quota set on
/// line 20, as a TOML reader would record it.
fn with_quota(body: &str, quota: &str) -> Vec<Lint> {
    let (mut cfg, mut map) = parse_pacerd_mapped(one(body).as_bytes()).unwrap();
    cfg.listeners[0].per_ip_conn_quota = Some(crate::ConnQuota::parse(quota).unwrap());
    map.listeners[0].set("per_ip_conn_quota", 20);
    cfg.validate().unwrap();
    lint(&cfg, &map)
}

#[test]
fn quota_unreachable_when_the_rate_admits_less_in_the_window() {
    // 1/min with a burst of 5 admits 5 + 1,440 in a day.
    let got = with_quota("per_ip_conn_rate 1/m burst 5\n", "1445/day");
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!((got[0].line, got[0].class), (20, Class::QuotaUnreachable));
    assert_eq!(
        got[0].message,
        "per_ip_conn_quota 1445/day is never reached: per_ip_conn_rate 1/m burst 5 admits at most 1445 new connections per client in any day, so the quota never refuses anyone"
    );
    // One fewer and the quota can refuse.
    assert_eq!(with_quota("per_ip_conn_rate 1/m burst 5\n", "1444/day"), []);
    // Windows that are not whole units, and refills that are not whole.
    let got = with_quota("per_ip_conn_rate 7/h\n", "17/90min");
    assert_eq!(got[0].class, Class::QuotaUnreachable);
    assert!(
        got[0]
            .message
            .contains("admits at most 17 new connections per client in any 90min")
    );
    assert_eq!(with_quota("per_ip_conn_rate 7/h\n", "16/90min"), []);
    // Without a rate the quota is the only limit.
    assert_eq!(with_quota("", "10/day"), []);
}

#[test]
fn a_quota_alone_is_per_client_admission() {
    // It keys the table, so table settings are used and a small table is
    // reported.
    let got = with_quota("max_conns 100\ntable_size 64\n", "10/day");
    assert_eq!(
        got.iter().map(|l| (l.line, l.class)).collect::<Vec<_>>(),
        [(5, Class::TableBelowMaxConns)]
    );
    let got = with_quota("table_full evict\n", "10/day");
    assert_eq!(got, []);
}

#[test]
fn serde_only_rate_periods_are_shown_in_whole_units() {
    let (mut cfg, map) = parse_pacerd_mapped(one("").as_bytes()).unwrap();
    cfg.listeners[0].bandwidth = Some(Rate::new(1 << 20, 20_000_000_000, 1024));
    let got = lint(&cfg, &map);
    assert_eq!(got.len(), 1);
    assert!(
        got[0]
            .message
            .starts_with("bandwidth 1048576/20s burst 1024: a burst of 1024 bytes"),
        "{}",
        got[0].message
    );
}
