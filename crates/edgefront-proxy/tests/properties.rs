//! Properties of path cleaning and client addresses, from steer's
//! `FuzzCleanPath` and `FuzzClientIP`, plus the re-encoding round trip.

use std::net::IpAddr;

use edgefront_core::config::IpPrefix;
use edgefront_proxy::forwarded::{client_ip, forwarded_chain, is_trusted, peer_addr};
use edgefront_proxy::path::{RequestTarget, clean_path, escape, unescape};
use proptest::prelude::*;

/// Path-ish bytes: mostly slashes, dots and escapes, some anything.
fn path_bytes() -> impl Strategy<Value = Vec<u8>> {
    let atom = prop_oneof![
        Just(b"/".to_vec()),
        Just(b"/".to_vec()),
        Just(b".".to_vec()),
        Just(b"..".to_vec()),
        Just(b"%2F".to_vec()),
        Just(b"%2e".to_vec()),
        Just(b"%25".to_vec()),
        Just(b"?".to_vec()),
        "[a-z]{1,3}".prop_map(String::into_bytes),
        any::<u8>().prop_map(|b| vec![b]),
    ];
    prop::collection::vec(atom, 0..12).prop_map(|v| v.concat())
}

fn remote() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("10.0.0.1:80".to_owned()),
        Just("[::1]:9".to_owned()),
        Just("8.8.8.8:1".to_owned()),
        Just("[::ffff:10.1.2.3]:4".to_owned()),
        "[0-9.:\\[\\]a-f%]{0,20}",
    ]
}

fn xff() -> impl Strategy<Value = String> {
    let entry = prop_oneof![
        Just("1.2.3.4".to_owned()),
        Just("10.0.0.2".to_owned()),
        Just("::1".to_owned()),
        Just("2001:db8::7".to_owned()),
        Just(" 10.9.9.9 ".to_owned()),
        "[0-9.:a-f ]{0,12}",
    ];
    prop::collection::vec(entry, 0..6).prop_map(|v| v.join(","))
}

fn trusted() -> Vec<IpPrefix> {
    vec!["10.0.0.0/8".parse().unwrap(), "::1/128".parse().unwrap()]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    /// steer's FuzzCleanPath: rooted, idempotent and free of dot segments
    /// and empty segments.
    #[test]
    fn clean_path_invariants(p in path_bytes()) {
        let c = clean_path(&p).into_owned();
        prop_assert!(c.starts_with(b"/"));
        prop_assert_eq!(&*clean_path(&c), &c[..]);
        let segments: Vec<&[u8]> = c.split(|&b| b == b'/').collect();
        for (i, seg) in segments.iter().enumerate() {
            prop_assert!(*seg != b"." && *seg != b"..");
            prop_assert!(!seg.is_empty() || i == 0 || i == segments.len() - 1);
        }
        prop_assert_eq!(c.ends_with(b"/"), p.ends_with(b"/") || c == b"/");
    }

    /// Go's escape is the inverse of its unescape, and its output needs no
    /// further escaping.
    #[test]
    fn escape_round_trips(p in prop::collection::vec(any::<u8>(), 0..40)) {
        let e = escape(&p);
        prop_assert!(e.iter().all(|&c| c.is_ascii_graphic() && c != b'?' && c != b'#'));
        prop_assert_eq!(&*unescape(&e).unwrap(), &p[..]);
    }

    /// Proxying the upstream target again changes nothing: it routes on the
    /// same cleaned path and goes upstream unchanged.
    #[test]
    fn upstream_target_is_a_fixed_point(p in path_bytes()) {
        let mut target = b"/".to_vec();
        target.extend_from_slice(&p);
        let Ok(t) = RequestTarget::parse(&target) else { return Ok(()) };
        let up = t.upstream_target().into_owned();
        let again = RequestTarget::parse(&up).unwrap();
        prop_assert_eq!(again.clean_path(), t.clean_path());
        prop_assert!(!again.was_cleaned());
        prop_assert_eq!(&*again.upstream_target(), &up[..]);
        prop_assert_eq!(again.query(), t.query());
    }

    /// steer's FuzzClientIP: the client is the peer or an address from the
    /// header, and an untrusted peer can never inject one.
    #[test]
    fn client_ip_invariants(remote in remote(), xff in xff()) {
        let trusted = trusted();
        let peer = peer_addr(&remote);
        let got = client_ip(peer, [xff.as_bytes()], &trusted);
        let Some(peer) = peer else {
            prop_assert_eq!(got, None);
            return Ok(());
        };
        let got: IpAddr = got.unwrap();
        if !is_trusted(peer, &trusted) {
            prop_assert_eq!(got, peer);
            return Ok(());
        }
        prop_assert!(got == peer || forwarded_chain([xff.as_bytes()]).contains(&got));
    }
}
