use std::net::IpAddr;

use proptest::prelude::*;

use super::*;

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

/// Two trusted proxies: a load balancer at 10.0.0.1 and an edge range
/// 192.0.2.0/24.
fn trusted() -> PrefixList<()> {
    PrefixList::parse("10.0.0.1\n192.0.2.0/24\n2001:db8:ffff::/48\n").unwrap()
}

fn xff(peer: &str, values: &[&str]) -> Resolved {
    resolve(ip(peer), &trusted(), Header::XForwardedFor, values)
}

fn fwd(peer: &str, values: &[&str]) -> Resolved {
    resolve(ip(peer), &trusted(), Header::Forwarded, values)
}

#[test]
fn an_untrusted_peer_is_the_client_whatever_it_sends() {
    let r = xff("198.51.100.9", &["203.0.113.5"]);
    assert_eq!(
        (r.client, r.hops, r.malformed),
        (ip("198.51.100.9"), 0, None)
    );
    let r = fwd("198.51.100.9", &["for=203.0.113.5"]);
    assert_eq!(r.client, ip("198.51.100.9"));
    // An IPv4-mapped peer is its IPv4 address, and trusted as such.
    let r = xff("::ffff:10.0.0.1", &["203.0.113.5"]);
    assert_eq!((r.client, r.hops), (ip("203.0.113.5"), 1));
}

#[test]
fn behind_two_trusted_proxies_the_rightmost_untrusted_entry_is_the_client() {
    // client 203.0.113.5 -> edge 192.0.2.7 -> balancer 10.0.0.1 -> us
    let r = xff("10.0.0.1", &["203.0.113.5, 192.0.2.7"]);
    assert_eq!((r.client, r.hops), (ip("203.0.113.5"), 2));
    let r = fwd("10.0.0.1", &["for=203.0.113.5, for=192.0.2.7"]);
    assert_eq!((r.client, r.hops), (ip("203.0.113.5"), 2));
    // Split over two header lines, same answer.
    let r = xff("10.0.0.1", &["203.0.113.5", "192.0.2.7"]);
    assert_eq!(r.client, ip("203.0.113.5"));
}

#[test]
fn spoofed_entries_to_the_left_are_never_reached() {
    // The client sends its own X-Forwarded-For claiming to be 10.9.9.9,
    // or even a trusted proxy; our edge appends the client's real address.
    for forged in ["10.9.9.9", "10.0.0.1", "192.0.2.50", "1.1.1.1, 2.2.2.2"] {
        let v = format!("{forged}, 203.0.113.5, 192.0.2.7");
        let r = xff("10.0.0.1", &[&v]);
        assert_eq!(r.client, ip("203.0.113.5"), "{forged}");
        let f = format!(
            "for={}, for=203.0.113.5, for=192.0.2.7",
            forged.replace(", ", ", for=")
        );
        assert_eq!(fwd("10.0.0.1", &[&f]).client, ip("203.0.113.5"), "{forged}");
    }
    // A client that connects straight to us cannot pose as anyone.
    assert_eq!(xff("203.0.113.5", &["10.0.0.1"]).client, ip("203.0.113.5"));
}

#[test]
fn all_trusted_means_the_leftmost_and_none_means_the_peer() {
    let r = xff("10.0.0.1", &["192.0.2.3, 192.0.2.7"]);
    assert_eq!((r.client, r.hops), (ip("192.0.2.3"), 2));
    let r = xff("10.0.0.1", &[]);
    assert_eq!((r.client, r.hops), (ip("10.0.0.1"), 0));
    let r = xff("10.0.0.1", &["", " , "]);
    assert_eq!(r.client, ip("10.0.0.1"));
}

#[test]
fn malformed_entries_stop_at_the_proxy_that_appended_them() {
    for (value, why) in [
        ("203.0.113.5, garbage", "\"garbage\" is not an address"),
        ("203.0.113.5, 192.0.2.300", "is not an address"),
        ("203.0.113.5, [2001:db8::1", "is not an address"),
    ] {
        let r = xff("10.0.0.1", &[value]);
        assert_eq!(r.client, ip("10.0.0.1"), "{value}");
        assert!(
            r.malformed.as_deref().unwrap().contains(why),
            "{value}: {r:?}"
        );
    }
    // A malformed entry left of an untrusted one is never reached.
    let r = xff("10.0.0.1", &["garbage, 203.0.113.5"]);
    assert_eq!((r.client, r.malformed), (ip("203.0.113.5"), None));
    // Past the edge proxy: the edge is the client of last resort.
    let r = xff("10.0.0.1", &["garbage, 192.0.2.7"]);
    assert_eq!((r.client, r.hops), (ip("192.0.2.7"), 1));
    assert!(r.malformed.is_some());
}

#[test]
fn xff_entries_take_ports_and_brackets() {
    assert_eq!(xff_entry("203.0.113.5:443").unwrap(), ip("203.0.113.5"));
    assert_eq!(
        xff_entry(" [2001:db8::1]:8080 ").unwrap(),
        ip("2001:db8::1")
    );
    assert_eq!(xff_entry("[2001:db8::1]").unwrap(), ip("2001:db8::1"));
    assert_eq!(xff_entry("2001:db8::1").unwrap(), ip("2001:db8::1"));
    for bad in [
        "203.0.113.5:",
        "203.0.113.5:99999",
        "[2001:db8::1]:x",
        "[1.2.3.4]",
        "unknown",
        "",
        "a:b:c",
    ] {
        assert!(xff_entry(bad).is_err(), "{bad}");
    }
}

#[test]
fn forwarded_follows_rfc_7239() {
    let one = |s: &str| forwarded_for(s);
    assert_eq!(
        one("for=192.0.2.60;proto=http;by=203.0.113.43").unwrap(),
        Some(ip("192.0.2.60"))
    );
    assert_eq!(
        one("For=\"[2001:db8:cafe::17]:4711\"").unwrap(),
        Some(ip("2001:db8:cafe::17"))
    );
    assert_eq!(
        one("for=\"192.0.2.43:47011\"").unwrap(),
        Some(ip("192.0.2.43"))
    );
    assert_eq!(
        one("for=192.0.2.43:_hidden").unwrap(),
        Some(ip("192.0.2.43"))
    );
    assert_eq!(one("proto=https;by=203.0.113.43").unwrap(), None);
    assert_eq!(one("").unwrap(), None);
    for (bad, why) in [
        ("for=unknown", "for=unknown"),
        ("for=_hidden", "obfuscated"),
        ("for=[2001:db8::1]", "must be quoted"),
        ("for=\"2001:db8::1", "unterminated quote"),
        ("for=\"[2001:db8::1\"", "no closing bracket"),
        ("for=\"a\\\"b\"", "escape"),
        ("for=1.2.3.4;for=5.6.7.8", "twice"),
        ("proto", "is not a parameter"),
        ("for=300.1.1.1", "is not an address"),
    ] {
        let e = one(bad).unwrap_err();
        assert!(e.contains(why), "{bad}: {e}");
    }
    // Quoted commas and semicolons do not split.
    let c = chain(
        Header::Forwarded,
        &["for=\"[2001:db8::1]\";ext=\"a,b;c\", for=192.0.2.1"],
    );
    assert_eq!(c, vec![Ok(ip("2001:db8::1")), Ok(ip("192.0.2.1"))]);
    assert_eq!(Header::Forwarded.name(), "forwarded");
    assert_eq!(Header::XForwardedFor.name(), "x-forwarded-for");
}

#[test]
fn unknown_or_obfuscated_hops_stop_at_the_proxy() {
    let r = fwd("10.0.0.1", &["for=203.0.113.5, for=unknown"]);
    assert_eq!(r.client, ip("10.0.0.1"));
    assert_eq!(r.malformed.as_deref(), Some("for=unknown"));
    let r = fwd("10.0.0.1", &["for=_abc, for=192.0.2.7"]);
    assert_eq!(r.client, ip("192.0.2.7"));
    // IPv6 through an IPv6 trusted proxy.
    let r = fwd("2001:db8:ffff::1", &["for=\"[2001:db8:1::5]\""]);
    assert_eq!(r.client, ip("2001:db8:1::5"));
}

proptest! {
    /// Whatever a client prepends, the answer is the entry our trusted
    /// proxies appended for it; and no input panics.
    #[test]
    fn prepending_never_changes_the_client(junk in "[ -~]{0,80}", n in 1usize..4) {
        let mut v = junk.clone();
        v.push_str(", 203.0.113.5");
        for i in 0..n {
            v.push_str(&format!(", 192.0.2.{i}"));
        }
        let r = xff("10.0.0.1", &[&v]);
        prop_assert_eq!(r.client, ip("203.0.113.5"));
        let _ = xff("10.0.0.1", &[&junk]);
        let _ = fwd("10.0.0.1", &[&junk]);
    }
}
