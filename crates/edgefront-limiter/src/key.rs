//! The key a client is limited under: its address, with IPv6 addresses
//! grouped by a prefix and IPv4-mapped IPv6 addresses taken as IPv4.
//!
//! An IPv6 client is usually given a whole /64, and often a /56 or a /48,
//! so it can rotate through more addresses than any table can hold. Keyed
//! by full address, each new address would be a new client with a full
//! bucket, and the client could multiply its quota without limit. Keyed
//! by its prefix, every address it rotates through is the same client.
//! [`Grouping`] is the prefix: per listener (`ipv6_prefix`) or per class
//! ([`crate::class::Class::ipv6_prefix`]), with [`key_for`] picking the
//! class's when the client has one.
//!
//! The key itself is [`ClientKey`]: 4 bytes for IPv4, including
//! `::ffff:a.b.c.d`, so a dual-stack listener cannot be made to count one
//! IPv4 client twice; 16 bytes for IPv6, cut to the prefix.

use std::net::IpAddr;

use crate::class::Classifier;
use crate::conn::ClientKey;

/// How many leading bits of an IPv6 address make one client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Grouping(u8);

impl Grouping {
    /// One client per /64, the usual end-site subnet.
    pub const SLASH_64: Grouping = Grouping(64);
    /// One client per /56, a common residential delegation.
    pub const SLASH_56: Grouping = Grouping(56);
    /// One client per /48, a whole site.
    pub const SLASH_48: Grouping = Grouping(48);

    /// Any prefix length from 1 to 128.
    pub fn bits(bits: u8) -> Option<Self> {
        (1..=128).contains(&bits).then_some(Self(bits))
    }

    pub fn prefix_len(self) -> u8 {
        self.0
    }

    /// The key of `ip` under this grouping.
    pub fn key(self, ip: IpAddr) -> ClientKey {
        ClientKey::new(ip, u32::from(self.0))
    }
}

/// The key of `ip`: under its class's grouping if `classes` gives it one,
/// otherwise under `default`.
pub fn key_for(classes: &Classifier, default: Grouping, ip: IpAddr) -> ClientKey {
    let g = classes
        .classify(ip)
        .and_then(|c| Grouping::bits(c.class.ipv6_prefix))
        .unwrap_or(default);
    g.key(ip)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::net::Ipv6Addr;

    use super::*;
    use crate::class::{Action, Class};
    use crate::gcra::Rate;
    use crate::table::{Table, TableConfig};

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn groupings_cut_ipv6_and_leave_ipv4_alone() {
        let a = ip("2001:db8:1:2:3:4:5:6");
        assert_eq!(Grouping::SLASH_64.key(a).ip(), ip("2001:db8:1:2::"));
        assert_eq!(Grouping::SLASH_56.key(a).ip(), ip("2001:db8:1::"));
        assert_eq!(Grouping::SLASH_48.key(a).ip(), ip("2001:db8:1::"));
        assert_eq!(
            Grouping::SLASH_56.key(ip("2001:db8:1:2ff::")).ip(),
            ip("2001:db8:1:200::")
        );
        assert_eq!(Grouping::bits(128).unwrap().key(a).ip(), a);
        assert_eq!(Grouping::bits(0), None);
        assert_eq!(Grouping::bits(129), None);
        assert_eq!(Grouping::SLASH_48.prefix_len(), 48);
        for g in [Grouping::SLASH_64, Grouping::SLASH_48] {
            assert_eq!(g.key(ip("192.0.2.7")), g.key(ip("::ffff:192.0.2.7")));
            assert_eq!(g.key(ip("192.0.2.7")).as_bytes().len(), 4);
        }
    }

    #[test]
    fn a_class_picks_its_own_grouping() {
        let mut isp = Class::new("isp", Action::Limit);
        isp.ipv6_prefix = 48;
        isp.prefixes = vec!["2001:db8::/32".parse().unwrap()];
        let c = Classifier::new(vec![isp]).unwrap();
        let k = |s| key_for(&c, Grouping::SLASH_64, ip(s));
        assert_eq!(k("2001:db8:1:2::1"), k("2001:db8:1:ffff::1"));
        assert_ne!(k("2001:db8:1::1"), k("2001:db8:2::1"));
        // Outside the class, the default /64.
        assert_ne!(k("2001:db9:0:1::1"), k("2001:db9:0:2::1"));
        assert_eq!(k("2001:db9:0:1::1"), k("2001:db9:0:1:ffff::1"));
    }

    /// Done when, item 3: a client rotating through 10,000 addresses of
    /// its /64 is one client, so its burst is spent once; keyed by full
    /// address it would have had 10,000 bursts.
    #[test]
    fn rotating_addresses_inside_the_group_do_not_multiply_a_quota() {
        const SEC: u64 = 1_000_000_000;
        for (g, base, step) in [
            (Grouping::SLASH_64, 0x2001_0db8_0001_0002u128 << 64, 1u128),
            (Grouping::SLASH_56, 0x2001_0db8_0001_0200u128 << 64, 1 << 64),
            (Grouping::SLASH_48, 0x2001_0db8_0001_0000u128 << 64, 1 << 72),
        ] {
            let t = Table::new(&TableConfig::new(1024, Rate::new(10, SEC, 10))).unwrap();
            let mut admitted = 0;
            let mut keys = HashSet::new();
            for i in 0..10_000u128 {
                // Within the group: the low bits and, for /56 and /48, the
                // subnet bits move.
                let a = IpAddr::V6(Ipv6Addr::from(base + (i % 256) * step + i));
                let k = g.key(a);
                keys.insert(k);
                if t.take(k.as_bytes(), SEC, 1).unwrap().allowed {
                    admitted += 1;
                }
            }
            assert_eq!(keys.len(), 1, "{g:?}");
            assert_eq!(admitted, 10, "{g:?}: the burst, once");
            assert_eq!(t.stats().entries, 1);
            // Keyed by full address the same rotation gets everything.
            // The table's hash seed is random, so it is sized for its
            // fullest shard: 10,000 keys in 16,384 slots filled one shard
            // about once in 14 runs.
            let t = Table::new(&TableConfig::new(65_536, Rate::new(10, SEC, 10))).unwrap();
            let full = Grouping::bits(128).unwrap();
            let free = (0..10_000u128)
                .filter(|i| {
                    let a = IpAddr::V6(Ipv6Addr::from(base + (i % 256) * step + i));
                    t.take(full.key(a).as_bytes(), SEC, 1).unwrap().allowed
                })
                .count();
            assert_eq!(free, 10_000);
        }
    }
}
