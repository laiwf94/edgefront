use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use proptest::prelude::*;

use super::*;

fn p(s: &str) -> Prefix {
    s.parse().unwrap()
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

fn list(entries: &[(&str, u32)]) -> PrefixList<u32> {
    let mut l = PrefixList::new();
    for (s, v) in entries {
        assert_eq!(l.insert(p(s), *v), None, "{s}");
    }
    l
}

fn find(l: &PrefixList<u32>, a: &str) -> Option<(String, u32)> {
    l.lookup(ip(a)).map(|m| (m.prefix.to_string(), *m.value))
}

#[test]
fn prefixes_parse_and_refuse_what_they_should() {
    assert_eq!(p("192.0.2.0/24").to_string(), "192.0.2.0/24");
    assert_eq!(p("192.0.2.7").to_string(), "192.0.2.7/32");
    assert_eq!(p("2001:db8::/32").to_string(), "2001:db8::/32");
    assert_eq!(p("::/0").len(), 0);
    assert!(p("0.0.0.0/0").is_empty());
    assert_eq!(p("::ffff:192.0.2.0/120").to_string(), "192.0.2.0/24");
    assert_eq!(p("::ffff:0:0/96").to_string(), "0.0.0.0/0");
    for (bad, why) in [
        ("192.0.2.1/24", "host bits are set past /24"),
        ("192.0.2.0/33", "the length is over 32"),
        ("2001:db8::1/32", "host bits are set past /32"),
        ("::/129", "the length is over 128"),
        ("192.0.2.0/", "is not a prefix length"),
        ("192.0.2.0/+8", "is not a prefix length"),
        ("192.0.2.0/300", "is not a prefix length"),
        ("192.0.2/24", "is not an IP address"),
        ("example.com/8", "is not an IP address"),
        ("", "is not an IP address"),
    ] {
        let e = bad.parse::<Prefix>().unwrap_err().to_string();
        assert!(e.contains(why), "{bad}: {e}");
    }
    let q = p("10.0.0.0/8");
    assert_eq!(q.addr(), ip("10.0.0.0"));
    assert!(q.contains(ip("10.255.0.1")));
    assert!(q.contains(ip("::ffff:10.1.2.3")));
    assert!(!q.contains(ip("11.0.0.0")));
    assert!(!q.contains(ip("2001:db8::1")));
    assert!(!p("::/0").contains(ip("10.0.0.1")));
}

#[test]
fn the_longest_prefix_wins() {
    let l = list(&[
        ("10.0.0.0/8", 1),
        ("10.1.0.0/16", 2),
        ("10.1.2.0/24", 3),
        ("10.1.2.3/32", 4),
        ("2001:db8::/32", 5),
        ("2001:db8:1::/48", 6),
    ]);
    assert_eq!(find(&l, "10.9.9.9"), Some(("10.0.0.0/8".into(), 1)));
    assert_eq!(find(&l, "10.1.9.9"), Some(("10.1.0.0/16".into(), 2)));
    assert_eq!(find(&l, "10.1.2.9"), Some(("10.1.2.0/24".into(), 3)));
    assert_eq!(find(&l, "10.1.2.3"), Some(("10.1.2.3/32".into(), 4)));
    assert_eq!(find(&l, "11.0.0.0"), None);
    assert_eq!(
        find(&l, "2001:db8:1:2::3"),
        Some(("2001:db8:1::/48".into(), 6))
    );
    assert_eq!(find(&l, "2001:db8:2::"), Some(("2001:db8::/32".into(), 5)));
    assert_eq!(find(&l, "2001:db9::"), None);
    // IPv4-mapped addresses are looked up as IPv4.
    assert_eq!(find(&l, "::ffff:10.1.2.3"), Some(("10.1.2.3/32".into(), 4)));
    assert_eq!(l.len(), 6);
    assert!(!l.is_empty());
}

#[test]
fn defaults_cover_everything_and_the_order_of_inserts_does_not_matter() {
    let mut a = list(&[("0.0.0.0/0", 0), ("::/0", 9), ("192.0.2.128/25", 1)]);
    assert_eq!(find(&a, "8.8.8.8"), Some(("0.0.0.0/0".into(), 0)));
    assert_eq!(find(&a, "192.0.2.200"), Some(("192.0.2.128/25".into(), 1)));
    assert_eq!(find(&a, "fe80::1"), Some(("::/0".into(), 9)));
    // Replacing a value keeps one entry.
    assert_eq!(a.insert(p("192.0.2.128/25"), 7), Some(1));
    assert_eq!(a.len(), 3);
    assert_eq!(a.get(&p("192.0.2.128/25")), Some(&7));
    assert_eq!(a.get(&p("192.0.2.0/25")), None);
    assert_eq!(a.get(&p("192.0.2.0/24")), None);
    // Splits in every order give the same answers.
    let prefixes = [
        "10.0.0.0/8",
        "10.128.0.0/9",
        "10.64.0.0/10",
        "10.96.0.0/11",
        "10.0.0.0/24",
    ];
    let addrs = [
        "10.0.0.1",
        "10.200.0.0",
        "10.70.0.0",
        "10.100.0.0",
        "10.1.0.0",
        "11.0.0.0",
    ];
    let reference: Vec<_> = {
        let l = list(&prefixes.iter().map(|s| (*s, 0)).collect::<Vec<_>>());
        addrs.iter().map(|a| find(&l, a).map(|m| m.0)).collect()
    };
    let mut order: Vec<usize> = (0..prefixes.len()).collect();
    for _ in 0..30 {
        order.rotate_left(1);
        order.swap(0, 2);
        let mut l = PrefixList::new();
        for &i in &order {
            l.insert(p(prefixes[i]), 0u32);
        }
        let got: Vec<_> = addrs.iter().map(|a| find(&l, a).map(|m| m.0)).collect();
        assert_eq!(got, reference, "{order:?}");
    }
}

#[test]
fn a_list_file_parses_with_comments_and_names_bad_lines() {
    let l = PrefixList::parse(
        "# office\n10.0.0.0/8\n\n  192.0.2.7   # one host\n2001:db8::/48\n10.0.0.0/8\n",
    )
    .unwrap();
    assert_eq!(l.len(), 3);
    assert!(l.contains(ip("10.1.1.1")));
    assert!(l.contains(ip("192.0.2.7")));
    assert!(!l.contains(ip("192.0.2.8")));
    assert!(l.contains(ip("2001:db8::99")));
    let iter: Vec<String> = l.iter().map(|(p, ())| p.to_string()).collect();
    assert_eq!(iter, ["10.0.0.0/8", "192.0.2.7/32", "2001:db8::/48"]);
    let e = PrefixList::parse("10.0.0.0/8\n\n10.0.0.1/8\n").unwrap_err();
    assert_eq!(
        e.to_string(),
        "line 3: 10.0.0.1/8: host bits are set past /8"
    );
    assert!(PrefixList::parse("").unwrap().is_empty());
    assert!(l.memory() > 0);
}

#[test]
fn a_swap_publishes_to_readers_without_blocking_them() {
    let s = Arc::new(Swap::new(1u32));
    let mut r = s.reader();
    assert_eq!(*r.get(), 1);
    let mut r2 = r.clone();
    s.store(2);
    assert_eq!(*r.get(), 2);
    assert_eq!(*r2.get(), 2);
    assert_eq!(*s.load(), 2);
    // Readers on other threads see every store, and a lookup in flight
    // keeps the value it holds.
    let held = s.load();
    let hs: Vec<_> = (0..4)
        .map(|_| {
            let mut r = s.reader();
            std::thread::spawn(move || {
                let mut last = 0;
                for _ in 0..10_000 {
                    let v = *r.get();
                    assert!(v >= last);
                    last = v;
                }
                last
            })
        })
        .collect();
    for v in 3..100 {
        s.store(v);
    }
    for h in hs {
        assert!(h.join().unwrap() >= 2);
    }
    assert_eq!(*held, 2);
    assert_eq!(*s.reader().get(), 99);
}

/// The reference: every stored prefix that contains the address, longest
/// first.
fn linear(entries: &[(Prefix, u32)], a: IpAddr) -> Option<(Prefix, u32)> {
    let mut best: Option<(Prefix, u32)> = None;
    for &(pr, v) in entries {
        if pr.contains(a) && best.is_none_or(|(b, _)| pr.len() > b.len()) {
            best = Some((pr, v));
        }
    }
    best
}

fn prefix_strategy() -> impl Strategy<Value = Prefix> {
    prop_oneof![
        (any::<u32>(), 0u8..=32).prop_map(|(a, l)| {
            Prefix::new(IpAddr::V4(Ipv4Addr::from(a.masked(l))), l).unwrap()
        }),
        // Few distinct high bits, so prefixes overlap often.
        (0u128..16, any::<u128>(), 0u8..=128).prop_map(|(top, a, l)| {
            let a = (top << 124) | (a >> 4);
            Prefix::new(IpAddr::V6(Ipv6Addr::from(a.masked(l))), l).unwrap()
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Done when, item 2's reference: longest-prefix match on random
    /// overlapping prefixes equals a linear scan, for addresses near and
    /// far from them.
    #[test]
    fn lookups_equal_a_linear_scan(
        prefixes in proptest::collection::vec(prefix_strategy(), 1..300),
        probes in proptest::collection::vec(any::<u128>(), 1..200),
    ) {
        let mut l = PrefixList::new();
        let mut entries: Vec<(Prefix, u32)> = Vec::new();
        for (i, pr) in prefixes.iter().enumerate() {
            let v = i as u32;
            if l.insert(*pr, v).is_some() {
                entries.retain(|(q, _)| q != pr);
            }
            entries.push((*pr, v));
        }
        let mut addrs: Vec<IpAddr> = Vec::new();
        for (i, x) in probes.iter().enumerate() {
            // Inside a stored prefix, IPv4, or IPv6 near the stored ones.
            let base = entries[i % entries.len()].0;
            addrs.push(match base.addr() {
                IpAddr::V4(b) => IpAddr::V4(Ipv4Addr::from(
                    u32::from(b) | ((*x as u32) & !u32::MAX.masked(base.len()))
                )),
                IpAddr::V6(b) => IpAddr::V6(Ipv6Addr::from(
                    u128::from(b) | (x & !u128::MAX.masked(base.len()))
                )),
            });
            addrs.push(IpAddr::V4(Ipv4Addr::from(*x as u32)));
            addrs.push(IpAddr::V6(Ipv6Addr::from(((x & 15) << 124) | (x >> 4))));
        }
        // Range edges: each prefix's first and last address and its
        // neighbours either side.
        for (pr, _) in &entries {
            match pr.addr() {
                IpAddr::V4(b) => {
                    let lo = u32::from(b);
                    let hi = lo | !u32::MAX.masked(pr.len());
                    for a in [lo.wrapping_sub(1), lo, hi, hi.wrapping_add(1)] {
                        addrs.push(IpAddr::V4(Ipv4Addr::from(a)));
                    }
                }
                IpAddr::V6(b) => {
                    let lo = u128::from(b);
                    let hi = lo | !u128::MAX.masked(pr.len());
                    for a in [lo.wrapping_sub(1), lo, hi, hi.wrapping_add(1)] {
                        addrs.push(IpAddr::V6(Ipv6Addr::from(a)));
                    }
                }
            }
        }
        // The tree first, then the compiled index: both equal the scan.
        prop_assert!(!l.is_compiled());
        for a in &addrs {
            let got = l.lookup(*a).map(|m| (m.prefix, *m.value));
            prop_assert_eq!(got, linear(&entries, *a), "tree {}", a);
        }
        l.compile();
        prop_assert!(l.is_compiled());
        for a in &addrs {
            let got = l.lookup(*a).map(|m| (m.prefix, *m.value));
            prop_assert_eq!(got, linear(&entries, *a), "compiled {}", a);
        }
        for (pr, v) in &entries {
            prop_assert_eq!(l.get(pr), Some(v));
        }
        // An insert drops the index; lookups stay right on the tree.
        l.insert(Prefix::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).unwrap(), u32::MAX);
        prop_assert!(!l.is_compiled());
    }
}

#[test]
fn the_compiled_index_covers_the_ends_of_the_address_space() {
    let mut l = list(&[
        ("0.0.0.0/0", 1),
        ("255.255.255.255", 2),
        ("0.0.0.0/32", 3),
        ("255.255.0.0/16", 4),
        ("::/0", 5),
        ("ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff", 6),
        ("::/128", 7),
    ]);
    l.compile();
    for (a, want) in [
        ("255.255.255.255", 2),
        ("255.255.255.254", 4),
        ("255.254.255.255", 1),
        ("0.0.0.0", 3),
        ("0.0.0.1", 1),
        ("ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff", 6),
        ("ffff:ffff:ffff:ffff:ffff:ffff:ffff:fffe", 5),
        ("::", 7),
        ("::2", 5),
    ] {
        assert_eq!(find(&l, a).map(|(_, v)| v), Some(want), "{a}");
    }
    let mut empty: PrefixList<u32> = PrefixList::new();
    empty.compile();
    assert!(empty.lookup(ip("10.0.0.1")).is_none());
    assert!(empty.lookup(ip("::1")).is_none());
    assert!(
        PrefixList::<()>::parse("10.0.0.0/8\n")
            .unwrap()
            .is_compiled()
    );
}

#[test]
fn a_million_prefixes_fit_and_answer() {
    let mut l = PrefixList::new();
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut kept = Vec::new();
    for i in 0..1_000_000u32 {
        let r = next();
        let len = 16 + (r % 17) as u8;
        let a = (r >> 32) as u32;
        let pr = Prefix::new(IpAddr::V4(Ipv4Addr::from(a.masked(len))), len).unwrap();
        l.insert(pr, i);
        if i % 1000 == 0 {
            kept.push(pr);
        }
    }
    assert!(l.len() > 900_000, "{}", l.len());
    // Two nodes per prefix at most.
    assert!(l.v4.nodes.len() <= 2 * l.len() + 1);
    for pr in kept {
        let m = l.lookup(pr.addr()).unwrap();
        assert!(m.prefix.len() >= pr.len());
        assert!(m.prefix.contains(pr.addr()));
    }
}

/// Lists packed into one block (every boundary shares its first 32 or 8
/// bits) still spread over the bucket table, and answer as the tree does.
#[test]
fn a_packed_list_compiles_to_the_answers_of_the_tree() {
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut tree = PrefixList::new();
    for i in 0..60_000u32 {
        let r = next();
        let pr = if i % 2 == 0 {
            let len = 33 + (r % 64) as u8;
            let a = (0x2001_0db8u128 << 96) | (u128::from(next()) << 32);
            Prefix::new(IpAddr::V6(Ipv6Addr::from(a.masked(len))), len).unwrap()
        } else {
            let len = 9 + (r % 24) as u8;
            let a = 0x0a00_0000 | ((r >> 40) as u32 & 0x00ff_ffff);
            Prefix::new(IpAddr::V4(Ipv4Addr::from(a.masked(len))), len).unwrap()
        };
        tree.insert(pr, i);
    }
    let mut compiled = PrefixList::new();
    for (pr, v) in tree.iter() {
        compiled.insert(pr, *v);
    }
    compiled.compile();
    let (r4, r6) = compiled.compiled.as_ref().unwrap();
    // The range after the last prefix may start just past the block
    // (2001:db9::, 11.0.0.0), so one bit fewer than the block's is shared.
    assert!(r6.skip >= 31 && r6.bits > 10, "{} {}", r6.skip, r6.bits);
    assert!(r4.skip >= 7 && r4.bits > 10, "{} {}", r4.skip, r4.bits);
    for _ in 0..200_000 {
        let r = next();
        let a = if r & 1 == 0 {
            IpAddr::V6(Ipv6Addr::from(
                (0x2001_0db8u128 << 96) | (u128::from(next()) << 32) | u128::from(r as u32),
            ))
        } else {
            IpAddr::V4(Ipv4Addr::from(
                0x0a00_0000 | ((r >> 8) as u32 & 0x00ff_ffff),
            ))
        };
        let want = tree.lookup(a).map(|m| (m.prefix, *m.value));
        assert_eq!(
            compiled.lookup(a).map(|m| (m.prefix, *m.value)),
            want,
            "{a}"
        );
    }
    assert!(compiled.memory() > tree.memory());
}
