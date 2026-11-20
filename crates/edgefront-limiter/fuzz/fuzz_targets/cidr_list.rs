//! Fuzzes prefix-list loading and lookup: any text as a list file, then
//! lookups of addresses the input picks.
//!
//! Every input must parse or be refused with a `line N:` error naming a
//! real line, without panicking; an accepted list must answer every
//! lookup with the longest of its prefixes that contains the address,
//! checked against a scan of the parsed prefixes.

#![no_main]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use edgefront_limiter::cidr::PrefixList;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (probes, text) = data.split_at(data.len().min(64));
    let Ok(text) = std::str::from_utf8(text) else {
        return;
    };
    let list = match PrefixList::parse(text) {
        Ok(l) => l,
        Err(e) => {
            let msg = e.to_string();
            let n: usize = msg
                .strip_prefix("line ")
                .and_then(|r| r.split(':').next())
                .and_then(|n| n.parse().ok())
                .expect("an error names its line");
            assert!(n >= 1 && n <= text.lines().count().max(1));
            return;
        }
    };
    let prefixes: Vec<_> = list.iter().map(|(p, ())| p).collect();
    assert_eq!(prefixes.len(), list.len());
    for chunk in probes.chunks(16) {
        let mut b = [0u8; 16];
        b[..chunk.len()].copy_from_slice(chunk);
        let addrs = [
            IpAddr::V4(Ipv4Addr::new(b[0], b[1], b[2], b[3])),
            IpAddr::V6(Ipv6Addr::from(b)),
        ];
        for a in addrs {
            let want = prefixes
                .iter()
                .filter(|p| p.contains(a))
                .max_by_key(|p| p.len())
                .copied();
            assert_eq!(list.lookup(a).map(|m| m.prefix), want);
        }
    }
});
