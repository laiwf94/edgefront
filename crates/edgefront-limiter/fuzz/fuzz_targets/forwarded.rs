//! Fuzzes the forwarding-header parsers and the trusted-hop walk:
//! `Forwarded` (RFC 7239) and `X-Forwarded-For` values of any bytes, split
//! into header lines at the input's newlines, behind a peer the input's
//! first byte picks.
//!
//! Every input must:
//! - parse without panicking, into one entry per hop the header names;
//! - resolve to the peer when the peer is not trusted, whatever the
//!   header says;
//! - resolve to an address that is the peer or one of the entries, never
//!   one the input did not name;
//! - take no more hops than there are entries, and stop with a reason at
//!   a malformed entry only after reaching it through trusted hops.

#![no_main]

use std::net::IpAddr;

use edgefront_limiter::cidr::PrefixList;
use edgefront_limiter::client_addr::{Header, chain, resolve};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&first, rest)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(rest) else {
        return;
    };
    let trusted = PrefixList::parse("10.0.0.0/8\n192.0.2.0/24\n2001:db8::/32\n").unwrap();
    let peers: [IpAddr; 4] = [
        "10.0.0.1".parse().unwrap(),
        "198.51.100.1".parse().unwrap(),
        "2001:db8::1".parse().unwrap(),
        "::ffff:10.1.2.3".parse().unwrap(),
    ];
    let peer = peers[usize::from(first) % peers.len()];
    let header = if first & 0x80 == 0 {
        Header::XForwardedFor
    } else {
        Header::Forwarded
    };
    let values: Vec<&str> = text.split('\n').collect();
    let entries = chain(header, &values);
    let r = resolve(peer, &trusted, header, &values);
    assert!(r.hops <= entries.len());
    if !trusted.contains(peer) {
        assert_eq!(r.client, peer);
        assert_eq!(r.hops, 0);
        assert!(r.malformed.is_none());
        return;
    }
    let named = entries.iter().filter_map(|e| e.as_ref().ok()).any(|a| {
        edgefront_limiter::cidr::canonical(*a) == r.client
    });
    assert!(r.client == edgefront_limiter::cidr::canonical(peer) || named);
    if r.malformed.is_some() {
        assert!(trusted.contains(r.client));
        assert!(entries.iter().any(|e| e.is_err()));
    }
});
