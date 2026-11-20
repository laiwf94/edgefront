//! Address policy: who the client is, what class it is in, and whether it
//! may connect, put together.
//!
//! [`AddressPolicy::decide`] takes the peer address and the forwarding
//! header's values and:
//!
//! 1. resolves the real client through the trusted proxies
//!    ([`crate::client_addr`]), when a header is configured;
//! 2. classifies it ([`crate::class::Classifier`]): the most specific
//!    prefix of any class decides, and an address no class lists gets the
//!    listener's own limits ([`Verdict::Default`]);
//! 3. for a class: refuses a denied client, admits an allowed one with no
//!    per-client limit, and holds a limited one to its class's connection
//!    cap and rate in a table of the class's own, keyed with the class's
//!    IPv6 grouping ([`crate::key`]).
//!
//! An admitted connection of a limited class holds a [`ClassPermit`],
//! which gives the hold back when dropped. A policy is built once from
//! its config ([`crate::config::AddressPolicyConfig`]) and replaced whole
//! on reload through [`crate::cidr::Swap`]; class tables start empty on a
//! reload, since its classes and limits may all have changed.

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::cidr::PrefixList;
use crate::class::{Action, Classifier};
use crate::client_addr::{self, Header, Resolved};
use crate::conn::{ClientKey, UNLIMITED};
use crate::gcra::Error;
use crate::key::Grouping;
use crate::table::{FullPolicy, Table, TableConfig};

/// A built policy. See the [module docs](self).
pub struct AddressPolicy {
    trusted: PrefixList<()>,
    header: Option<Header>,
    classifier: Classifier,
    /// One table per class, `None` for classes that do not limit.
    tables: Vec<Option<Table>>,
    default_grouping: Grouping,
    /// How often each [`Verdict`] was given, by [`Verdict::name`] order.
    counts: [AtomicU64; Verdict::NAMES.len()],
    /// Decisions where a trusted proxy sent a malformed hop.
    malformed: AtomicU64,
}

impl fmt::Debug for AddressPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AddressPolicy")
            .field("trusted", &self.trusted.len())
            .field("header", &self.header)
            .field("classes", &self.classifier.classes().len())
            .field("prefixes", &self.classifier.prefixes())
            .finish_non_exhaustive()
    }
}

/// What the policy decided for one connection or request.
#[derive(Debug)]
pub enum Verdict {
    /// No class covers the client: apply the listener's own limits.
    Default,
    /// The client's class allows it, with no per-client limit.
    Allow,
    /// The client's class denies it.
    Deny,
    /// The client's class limits it, and it is within its limits.
    Admitted(ClassPermit),
    /// The client's class limits it, and it is at its connection cap.
    OverCap,
    /// The client's class limits it, and its connection rate has no token;
    /// the next one comes after this many nanoseconds.
    OverRate(u64),
    /// The class's table has no room for a new client.
    TableFull,
}

impl Verdict {
    /// The verdicts' names, as counted and logged.
    pub const NAMES: [&'static str; 7] = [
        "default",
        "class_allow",
        "class_deny",
        "class_admitted",
        "class_conns",
        "class_rate",
        "class_table_full",
    ];

    /// Its name in [`Verdict::NAMES`].
    pub fn name(&self) -> &'static str {
        Self::NAMES[self.index()]
    }

    fn index(&self) -> usize {
        match self {
            Verdict::Default => 0,
            Verdict::Allow => 1,
            Verdict::Deny => 2,
            Verdict::Admitted(_) => 3,
            Verdict::OverCap => 4,
            Verdict::OverRate(_) => 5,
            Verdict::TableFull => 6,
        }
    }
}

/// A decision with what led to it.
#[derive(Debug)]
pub struct Decided {
    /// The client as resolved through the trusted proxies.
    pub resolved: Resolved,
    /// The class's name, or `None` for the default.
    pub class: Option<String>,
    pub verdict: Verdict,
}

/// One admitted connection's hold in its class's table.
pub struct ClassPermit {
    policy: Arc<AddressPolicy>,
    class: usize,
    key: ClientKey,
}

impl fmt::Debug for ClassPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClassPermit")
            .field("class", &self.class)
            .field("key", &self.key)
            .finish()
    }
}

impl Drop for ClassPermit {
    fn drop(&mut self) {
        if let Some(t) = &self.policy.tables[self.class] {
            let released = t.release(self.key.as_bytes());
            debug_assert!(released.is_ok(), "class table lost a hold");
        }
    }
}

impl AddressPolicy {
    /// A policy over `classifier`, with `table_size` entries per limited
    /// class, trusting the proxies in `trusted` to set `header`.
    pub fn new(
        trusted: PrefixList<()>,
        header: Option<Header>,
        classifier: Classifier,
        table_size: u32,
        default_grouping: Grouping,
    ) -> Result<Self, Error> {
        let tables = classifier
            .classes()
            .iter()
            .map(|c| {
                if c.action != Action::Limit {
                    return Ok(None);
                }
                let mut tc =
                    TableConfig::new(table_size, c.limits.per_ip_conn_rate.unwrap_or(UNLIMITED));
                tc.full_policy = FullPolicy::Deny;
                Table::new(&tc).map(Some)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            trusted,
            header,
            classifier,
            tables,
            default_grouping,
            counts: Default::default(),
            malformed: AtomicU64::new(0),
        })
    }

    pub fn classifier(&self) -> &Classifier {
        &self.classifier
    }

    /// The header that names the client behind trusted proxies, if any.
    pub fn header(&self) -> Option<Header> {
        self.header
    }

    /// The trusted proxies.
    pub fn trusted(&self) -> &PrefixList<()> {
        &self.trusted
    }

    /// Decides for a connection from `peer` carrying `values` of the
    /// configured header, at `now_ns`.
    pub fn decide(self: &Arc<Self>, peer: IpAddr, values: &[&str], now_ns: u64) -> Decided {
        let resolved = match self.header {
            Some(h) => client_addr::resolve(peer, &self.trusted, h, values),
            None => Resolved {
                client: crate::cidr::canonical(peer),
                hops: 0,
                malformed: None,
            },
        };
        let (class, verdict) = match self.classifier.classify(resolved.client) {
            None => (None, Verdict::Default),
            Some(c) => (
                Some(c.class.name.clone()),
                self.class_verdict(&c, resolved.client, now_ns),
            ),
        };
        self.counts[verdict.index()].fetch_add(1, Ordering::Relaxed);
        if resolved.malformed.is_some() {
            self.malformed.fetch_add(1, Ordering::Relaxed);
        }
        Decided {
            resolved,
            class,
            verdict,
        }
    }

    /// Each verdict's count since the policy was built, then
    /// `malformed_hop`: decisions where a trusted proxy's hop was not an
    /// address.
    pub fn counts(&self) -> Vec<(&'static str, u64)> {
        Verdict::NAMES
            .iter()
            .zip(&self.counts)
            .map(|(n, c)| (*n, c.load(Ordering::Relaxed)))
            .chain([("malformed_hop", self.malformed.load(Ordering::Relaxed))])
            .collect()
    }

    fn class_verdict(
        self: &Arc<Self>,
        c: &crate::class::Classified<'_>,
        client: IpAddr,
        now_ns: u64,
    ) -> Verdict {
        match c.class.action {
            Action::Allow => Verdict::Allow,
            Action::Deny => Verdict::Deny,
            Action::Limit => {
                let g = Grouping::bits(c.class.ipv6_prefix).unwrap_or(self.default_grouping);
                let key = g.key(client);
                let cap = c.class.limits.per_ip_max_conns.unwrap_or(u32::MAX);
                let t = self.tables[c.index]
                    .as_ref()
                    .expect("a limited class has a table");
                match t.hold(key.as_bytes(), now_ns, cap) {
                    Ok(d) if d.allowed => Verdict::Admitted(ClassPermit {
                        policy: Arc::clone(self),
                        class: c.index,
                        key,
                    }),
                    Ok(d) => Verdict::OverRate(d.retry_after_ns),
                    Err(Error::HoldLimit) => Verdict::OverCap,
                    Err(_) => Verdict::TableFull,
                }
            }
        }
    }

    /// Bytes the policy's lists and class tables take.
    pub fn memory(&self) -> usize {
        self.trusted.memory()
            + self.classifier.memory()
            + self
                .tables
                .iter()
                .flatten()
                .map(Table::memory)
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cidr::Prefix;
    use crate::class::Class;
    use crate::gcra::Rate;

    const SEC: u64 = 1_000_000_000;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn policy() -> Arc<AddressPolicy> {
        let mut deny = Class::new("blocked", Action::Deny);
        deny.prefixes = vec!["198.51.100.0/24".parse::<Prefix>().unwrap()];
        let mut allow = Class::new("office", Action::Allow);
        allow.prefixes = vec!["198.51.100.10".parse().unwrap()];
        let mut lim = Class::new("partners", Action::Limit);
        lim.prefixes = vec![
            "203.0.113.0/24".parse().unwrap(),
            "2001:db8::/32".parse().unwrap(),
        ];
        lim.ipv6_prefix = 48;
        lim.limits.per_ip_max_conns = Some(2);
        lim.limits.per_ip_conn_rate = Some(Rate::new(3, SEC, 3));
        Arc::new(
            AddressPolicy::new(
                PrefixList::parse("10.0.0.1\n").unwrap(),
                Some(Header::XForwardedFor),
                Classifier::new(vec![deny, allow, lim]).unwrap(),
                1024,
                Grouping::SLASH_64,
            )
            .unwrap(),
        )
    }

    #[test]
    fn each_class_does_what_it_says() {
        let p = policy();
        let d = p.decide(ip("10.0.0.1"), &["198.51.100.7"], SEC);
        assert!(matches!(d.verdict, Verdict::Deny));
        assert_eq!(d.class.as_deref(), Some("blocked"));
        assert_eq!(d.resolved.client, ip("198.51.100.7"));
        assert!(matches!(
            p.decide(ip("10.0.0.1"), &["198.51.100.10"], SEC).verdict,
            Verdict::Allow
        ));
        assert!(matches!(
            p.decide(ip("192.0.2.1"), &[], SEC).verdict,
            Verdict::Default
        ));
        // The partner: two at once, three a second.
        let a = p.decide(ip("10.0.0.1"), &["203.0.113.5"], SEC);
        let b = p.decide(ip("10.0.0.1"), &["203.0.113.5"], SEC);
        assert!(matches!(a.verdict, Verdict::Admitted(_)));
        assert!(matches!(b.verdict, Verdict::Admitted(_)));
        assert!(matches!(
            p.decide(ip("10.0.0.1"), &["203.0.113.5"], SEC).verdict,
            Verdict::OverCap
        ));
        drop(a);
        let c = p.decide(ip("10.0.0.1"), &["203.0.113.5"], SEC);
        assert!(matches!(c.verdict, Verdict::Admitted(_)));
        drop((b, c));
        match p.decide(ip("10.0.0.1"), &["203.0.113.5"], SEC).verdict {
            Verdict::OverRate(retry) => assert_eq!(retry, SEC / 3 + 1),
            v => panic!("{v:?}"),
        }
        // Another partner address has its own bucket.
        assert!(matches!(
            p.decide(ip("10.0.0.1"), &["203.0.113.6"], SEC).verdict,
            Verdict::Admitted(_)
        ));
        // IPv6 partners are grouped by /48.
        let v1 = p.decide(ip("2001:db8:1:1::1"), &[], SEC);
        let v2 = p.decide(ip("2001:db8:1:2::1"), &[], SEC);
        assert!(matches!(v1.verdict, Verdict::Admitted(_)));
        assert!(matches!(v2.verdict, Verdict::Admitted(_)));
        assert!(matches!(
            p.decide(ip("2001:db8:1:3::1"), &[], SEC).verdict,
            Verdict::OverCap
        ));
        assert!(p.memory() > 0);
        assert!(format!("{p:?}").contains("classes: 3"));
    }

    #[test]
    fn a_spoofed_header_from_an_untrusted_peer_is_ignored() {
        let p = policy();
        // The client claims to be the office through its own header.
        let d = p.decide(ip("198.51.100.7"), &["198.51.100.10"], SEC);
        assert!(matches!(d.verdict, Verdict::Deny));
        assert_eq!(d.resolved.client, ip("198.51.100.7"));
    }

    #[test]
    fn without_a_header_the_peer_is_the_client_and_a_full_table_refuses() {
        let mut lim = Class::new("tiny", Action::Limit);
        lim.prefixes = vec!["10.0.0.0/8".parse().unwrap()];
        lim.limits.per_ip_max_conns = Some(1);
        let p = Arc::new(
            AddressPolicy::new(
                PrefixList::new(),
                None,
                Classifier::new(vec![lim]).unwrap(),
                16,
                Grouping::SLASH_64,
            )
            .unwrap(),
        );
        let mut held = Vec::new();
        for i in 0..64u8 {
            let d = p.decide(IpAddr::from([10, 0, 0, i]), &["192.0.2.1"], SEC);
            assert_eq!(d.resolved.client, IpAddr::from([10, 0, 0, i]));
            match d.verdict {
                Verdict::Admitted(h) => held.push(h),
                Verdict::TableFull => break,
                v => panic!("{v:?}"),
            }
        }
        assert!(held.len() >= 16 && held.len() < 64, "{}", held.len());
        assert!(format!("{:?}", held[0]).contains("class: 0"));
    }
}
