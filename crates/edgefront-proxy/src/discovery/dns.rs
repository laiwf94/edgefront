//! Endpoints from DNS: a name's A and AAAA records on a fixed port, or a
//! service's SRV records, each looked up again when its TTL runs out.
//!
//! - **A and AAAA.** Both are asked at once. Every address becomes an
//!   endpoint on the configured port, with weight 1 and priority 0.
//! - **SRV.** Each record names a target and a port. A target's addresses
//!   come from the answer's additional section when the server sent them,
//!   and from A and AAAA lookups otherwise. A target of `.` means the
//!   service is not offered there (RFC 2782) and is skipped.
//! - **SRV priority.** SRV prefers the lowest priority, as edgefront does,
//!   but has 65,536 of them to edgefront's eight. The distinct priorities in
//!   an answer are ranked from the lowest, and the rank is the endpoint's
//!   priority, so `10, 20, 30` become `0, 1, 2`; every rank past 7 shares
//!   priority 7.
//! - **SRV weight.** Weights are relative within a priority. A weight of 0
//!   becomes 1, the smallest edgefront weight (RFC 2782 gives such targets
//!   "a very small chance"). When the largest weight is over 1,000 every
//!   weight is scaled into 1 to 1,000 by the same factor, rounding to the
//!   nearest and never below 1.
//! - **TTLs.** The set is looked up again when the shortest TTL among the
//!   records it came from runs out, but never sooner than
//!   [`DnsSource::min_interval`] or later than [`DnsSource::max_interval`].
//!   The resolver's cache is cleared before each lookup, so an answer is
//!   never served past its TTL, and a removed record stops taking traffic
//!   within one TTL plus the lookup.
//! - **No records.** A name that does not exist, or has no records of the
//!   type asked, is a successful empty answer: the source's set empties and
//!   the name is asked again after its negative TTL (the SOA minimum), or
//!   `min_interval` when there is none.
//! - **Failures.** A timeout, refusal or server failure is sent as a
//!   failure, so the merger keeps the last good set, and the lookup is
//!   retried after [`DnsSource::retry`], doubling to `max_interval`. A
//!   partial answer (A worked, AAAA timed out) is a failure too: sending it
//!   could drop endpoints that still exist.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolverConfig, ResolverOpts};
use hickory_resolver::net::NetError;
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::{Name, RData, Record, RecordType};
use hickory_resolver::{Resolver, TokioResolver};
use tokio::sync::mpsc;

use super::{Endpoint, Source, UpdateKind};

/// What a [`DnsSource`] looks up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsQuery {
    /// The A and AAAA records of `name`, each address on `port`.
    Host { name: String, port: u16 },
    /// The SRV records of `name`, such as `_http._tcp.api.internal`.
    Srv { name: String },
}

/// A discovery source that resolves a name.
#[derive(Debug, Clone)]
pub struct DnsSource {
    pub query: DnsQuery,
    /// The servers to ask; the system's (`/etc/resolv.conf`) when empty.
    pub servers: Vec<SocketAddr>,
    /// The shortest time between two lookups, whatever the TTL says.
    pub min_interval: Duration,
    /// The longest time an answer is kept before it is looked up again.
    pub max_interval: Duration,
    /// The first wait after a failed lookup; it doubles on each failure in
    /// a row, up to `max_interval`.
    pub retry: Duration,
    /// How long one lookup may take.
    pub timeout: Duration,
}

impl DnsSource {
    /// A source for `query` with the defaults: the system's servers, at
    /// most one lookup every 500 ms, at least one every 5 minutes, retries
    /// from 1 s and a 2 s lookup timeout.
    pub fn new(query: DnsQuery) -> DnsSource {
        DnsSource {
            query,
            servers: Vec::new(),
            min_interval: Duration::from_millis(500),
            max_interval: Duration::from_secs(300),
            retry: Duration::from_secs(1),
            timeout: Duration::from_secs(2),
        }
    }

    /// Asks `servers` instead of the system's.
    pub fn servers(mut self, servers: Vec<SocketAddr>) -> DnsSource {
        self.servers = servers;
        self
    }

    fn resolver(&self) -> Result<TokioResolver, String> {
        let builder = if self.servers.is_empty() {
            Resolver::builder_tokio().map_err(|e| format!("system resolver: {e}"))?
        } else {
            let servers = self
                .servers
                .iter()
                .map(|s| {
                    let mut udp = ConnectionConfig::udp();
                    udp.port = s.port();
                    let mut tcp = ConnectionConfig::tcp();
                    tcp.port = s.port();
                    NameServerConfig::new(s.ip(), true, vec![udp, tcp])
                })
                .collect();
            Resolver::builder_with_config(
                ResolverConfig::from_name_servers(servers),
                TokioRuntimeProvider::default(),
            )
        };
        let mut opts = ResolverOpts::default();
        opts.timeout = self.timeout;
        opts.attempts = 1;
        opts.ndots = 0;
        builder
            .with_options(opts)
            .build()
            .map_err(|e| format!("resolver: {e}"))
    }

    async fn watch(self, tx: mpsc::Sender<UpdateKind>) {
        let resolver = match self.resolver() {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(UpdateKind::Failed(e)).await;
                return;
            }
        };
        let mut sent: Option<Vec<Endpoint>> = None;
        let mut backoff = self.retry;
        loop {
            resolver.clear_cache();
            let now = Instant::now();
            let wait = match self.lookup(&resolver).await {
                Ok(answer) => {
                    backoff = self.retry;
                    if sent.as_ref() != Some(&answer.endpoints) {
                        if tx
                            .send(UpdateKind::Set(answer.endpoints.clone()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        sent = Some(answer.endpoints);
                    }
                    answer
                        .valid_until
                        .saturating_duration_since(now)
                        .clamp(self.min_interval, self.max_interval)
                }
                Err(e) => {
                    // A failure is sent every time: each one is counted.
                    // The next good answer is sent even when it equals the
                    // last, so the merger sees the source recover.
                    if tx.send(UpdateKind::Failed(e)).await.is_err() {
                        return;
                    }
                    sent = None;
                    let wait = backoff;
                    backoff = (backoff * 2).min(self.max_interval);
                    wait
                }
            };
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = tx.closed() => return,
            }
        }
    }

    async fn lookup(&self, resolver: &TokioResolver) -> Result<Answer, String> {
        match &self.query {
            DnsQuery::Host { name, port } => {
                let (addrs, valid_until) = self.addresses(resolver, name).await?;
                let endpoints = addrs
                    .into_iter()
                    .map(|ip| Endpoint::new(SocketAddr::new(ip, *port).to_string()))
                    .collect();
                Ok(Answer {
                    endpoints,
                    valid_until,
                })
            }
            DnsQuery::Srv { name } => self.srv(resolver, name).await,
        }
    }

    /// The A and AAAA addresses of `name`, in answer order with A first,
    /// and when the shortest of their TTLs runs out.
    async fn addresses(
        &self,
        resolver: &TokioResolver,
        name: &str,
    ) -> Result<(Vec<IpAddr>, Instant), String> {
        let (a, aaaa) = tokio::join!(
            resolver.lookup(name, RecordType::A),
            resolver.lookup(name, RecordType::AAAA)
        );
        let mut addrs = Vec::new();
        let mut valid_until: Option<Instant> = None;
        for result in [a, aaaa] {
            match records(result, self.min_interval)? {
                Records::Found(list, until) => {
                    valid_until = Some(valid_until.map_or(until, |v| v.min(until)));
                    for r in list.answers {
                        match &r.data {
                            RData::A(a) => addrs.push(IpAddr::V4(a.0)),
                            RData::AAAA(a) => addrs.push(IpAddr::V6(a.0)),
                            _ => {}
                        }
                    }
                }
                Records::None(until) => {
                    valid_until = Some(valid_until.map_or(until, |v| v.min(until)));
                }
            }
        }
        dedup(&mut addrs);
        let until = valid_until.unwrap_or_else(|| Instant::now() + self.min_interval);
        Ok((addrs, until))
    }

    async fn srv(&self, resolver: &TokioResolver, name: &str) -> Result<Answer, String> {
        let (list, mut valid_until, additionals) = match records(
            resolver.lookup(name, RecordType::SRV).await,
            self.min_interval,
        )? {
            Records::Found(list, until) => {
                let extra = list.additionals;
                (list.answers, until, extra)
            }
            Records::None(until) => {
                return Ok(Answer {
                    endpoints: Vec::new(),
                    valid_until: until,
                });
            }
        };
        let mut targets: Vec<SrvTarget> = Vec::new();
        for r in &list {
            if let RData::SRV(s) = &r.data {
                if s.target.is_root() {
                    continue;
                }
                targets.push(SrvTarget {
                    priority: s.priority,
                    weight: s.weight,
                    port: s.port,
                    target: s.target.clone(),
                });
            }
        }
        // Addresses the server put in the additional section, by name.
        let mut glue: HashMap<Name, Vec<IpAddr>> = HashMap::new();
        for r in &additionals {
            let ip = match &r.data {
                RData::A(a) => IpAddr::V4(a.0),
                RData::AAAA(a) => IpAddr::V6(a.0),
                _ => continue,
            };
            let entry = glue.entry(r.name.to_lowercase()).or_default();
            entry.push(ip);
            valid_until = valid_until.min(Instant::now() + Duration::from_secs(r.ttl.into()));
        }
        let mut resolved: HashMap<Name, Vec<IpAddr>> = HashMap::new();
        for t in &targets {
            let key = t.target.to_lowercase();
            if glue.contains_key(&key) || resolved.contains_key(&key) {
                continue;
            }
            let (addrs, until) = self.addresses(resolver, &t.target.to_ascii()).await?;
            valid_until = valid_until.min(until);
            resolved.insert(key, addrs);
        }
        let endpoints = srv_endpoints(&targets, |name| {
            let key = name.to_lowercase();
            glue.get(&key)
                .or_else(|| resolved.get(&key))
                .cloned()
                .unwrap_or_default()
        });
        Ok(Answer {
            endpoints,
            valid_until,
        })
    }
}

impl Source for DnsSource {
    fn name(&self) -> String {
        match &self.query {
            DnsQuery::Host { name, port } => format!("dns:{name}:{port}"),
            DnsQuery::Srv { name } => format!("srv:{name}"),
        }
    }

    fn run(
        self: Box<Self>,
        tx: mpsc::Sender<UpdateKind>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin((*self).watch(tx))
    }
}

/// One lookup's endpoints and when they should be looked up again.
struct Answer {
    endpoints: Vec<Endpoint>,
    valid_until: Instant,
}

/// The records of one lookup, split by section.
struct Sections {
    answers: Vec<Record>,
    additionals: Vec<Record>,
}

enum Records {
    /// Records, and when the lookup's TTL runs out.
    Found(Sections, Instant),
    /// No records; ask again at this instant.
    None(Instant),
}

/// Sorts a lookup result into records, a successful empty answer, or a
/// failure.
fn records(
    result: Result<hickory_resolver::lookup::Lookup, NetError>,
    floor: Duration,
) -> Result<Records, String> {
    match result {
        Ok(lookup) => {
            let sections = Sections {
                answers: lookup.answers().to_vec(),
                additionals: lookup.additionals().to_vec(),
            };
            Ok(Records::Found(sections, lookup.valid_until()))
        }
        Err(e) if e.is_no_records_found() || e.is_nx_domain() => {
            let ttl = match &e {
                NetError::Dns(hickory_resolver::net::DnsError::NoRecordsFound(n)) => {
                    n.negative_ttl.map(|t| Duration::from_secs(t.into()))
                }
                _ => None,
            };
            Ok(Records::None(Instant::now() + ttl.unwrap_or(floor)))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Drops repeated addresses, keeping the first of each.
fn dedup(addrs: &mut Vec<IpAddr>) {
    let mut seen = std::collections::HashSet::new();
    addrs.retain(|a| seen.insert(*a));
}

/// One SRV record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrvTarget {
    pub priority: u16,
    pub weight: u16,
    pub port: u16,
    pub target: Name,
}

/// The endpoints for `targets`, with each target's addresses from
/// `addresses`: SRV priorities ranked into edgefront's (see the module
/// documentation), weights mapped into 1 to 1,000, and an address that two
/// records reach kept once, from the first.
pub fn srv_endpoints(
    targets: &[SrvTarget],
    mut addresses: impl FnMut(&Name) -> Vec<IpAddr>,
) -> Vec<Endpoint> {
    let mut priorities: Vec<u16> = targets.iter().map(|t| t.priority).collect();
    priorities.sort_unstable();
    priorities.dedup();
    let max_weight = targets.iter().map(|t| t.weight).max().unwrap_or(0);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut order: Vec<&SrvTarget> = targets.iter().collect();
    order.sort_by_key(|t| t.priority);
    for t in order {
        let rank = priorities
            .binary_search(&t.priority)
            .expect("every priority is listed");
        let priority = u8::try_from(rank.min(7)).expect("at most 7");
        let weight = srv_weight(t.weight, max_weight);
        for ip in addresses(&t.target) {
            let address = SocketAddr::new(ip, t.port).to_string();
            if !seen.insert(address.clone()) {
                continue;
            }
            let mut ep = Endpoint::new(address);
            ep.weight = weight;
            ep.priority = priority;
            out.push(ep);
        }
    }
    out
}

/// An SRV weight in edgefront's 1 to 1,000, given the largest weight in
/// the answer.
pub fn srv_weight(weight: u16, max: u16) -> u32 {
    let w = u32::from(weight);
    let max = u32::from(max);
    let scaled = if max > 1000 {
        (w * 1000 + max / 2) / max
    } else {
        w
    };
    scaled.clamp(1, 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn target(priority: u16, weight: u16, port: u16, name: &str) -> SrvTarget {
        SrvTarget {
            priority,
            weight,
            port,
            target: Name::from_ascii(name).unwrap(),
        }
    }

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
    }

    #[test]
    fn weights_map_into_one_to_a_thousand() {
        assert_eq!(srv_weight(0, 10), 1);
        assert_eq!(srv_weight(5, 10), 5);
        assert_eq!(srv_weight(1000, 1000), 1000);
        assert_eq!(srv_weight(65535, 65535), 1000);
        assert_eq!(srv_weight(32768, 65535), 500);
        assert_eq!(srv_weight(1, 65535), 1);
        assert_eq!(srv_weight(0, 0), 1);
    }

    #[test]
    fn priorities_are_ranked_from_the_lowest() {
        let targets = [
            target(30, 1, 80, "c."),
            target(10, 1, 80, "a."),
            target(20, 1, 80, "b."),
        ];
        let eps = srv_endpoints(&targets, |n| match n.to_ascii().as_str() {
            "a." => vec![ip(1)],
            "b." => vec![ip(2)],
            _ => vec![ip(3)],
        });
        let got: Vec<(&str, u8)> = eps.iter().map(|e| (&*e.address, e.priority)).collect();
        assert_eq!(
            got,
            [("10.0.0.1:80", 0), ("10.0.0.2:80", 1), ("10.0.0.3:80", 2)]
        );
    }

    #[test]
    fn ranks_past_seven_share_priority_seven() {
        let targets: Vec<SrvTarget> = (0..10u8)
            .map(|i| target(u16::from(i) * 5, 1, 80, &format!("t{i}.")))
            .collect();
        let eps = srv_endpoints(&targets, |n| {
            let i: u8 = n.to_ascii()[1..2].parse().unwrap();
            vec![ip(i + 1)]
        });
        let prios: Vec<u8> = eps.iter().map(|e| e.priority).collect();
        assert_eq!(prios, [0, 1, 2, 3, 4, 5, 6, 7, 7, 7]);
    }

    #[test]
    fn each_address_of_a_target_gets_its_port_and_weight() {
        let targets = [target(0, 3, 8443, "api."), target(0, 0, 9000, "spare.")];
        let eps = srv_endpoints(&targets, |n| {
            if n.to_ascii() == "api." {
                vec![ip(1), "fd00::1".parse().unwrap()]
            } else {
                vec![ip(9)]
            }
        });
        let got: Vec<(&str, u32)> = eps.iter().map(|e| (&*e.address, e.weight)).collect();
        assert_eq!(
            got,
            [
                ("10.0.0.1:8443", 3),
                ("[fd00::1]:8443", 3),
                ("10.0.0.9:9000", 1)
            ]
        );
    }

    #[test]
    fn an_address_two_records_reach_is_kept_from_the_preferred_one() {
        let targets = [target(5, 7, 80, "backup."), target(1, 2, 80, "main.")];
        let eps = srv_endpoints(&targets, |_| vec![ip(1)]);
        assert_eq!(eps.len(), 1);
        assert_eq!((eps[0].priority, eps[0].weight), (0, 2));
    }

    #[test]
    fn a_target_without_addresses_adds_nothing() {
        let targets = [target(0, 1, 80, "gone.")];
        assert!(srv_endpoints(&targets, |_| Vec::new()).is_empty());
    }

    #[test]
    fn addresses_are_deduplicated_in_order() {
        let mut a = vec![ip(2), ip(1), ip(2), ip(3), ip(1)];
        dedup(&mut a);
        assert_eq!(a, [ip(2), ip(1), ip(3)]);
    }

    #[test]
    fn names_say_what_is_resolved() {
        let host = DnsSource::new(DnsQuery::Host {
            name: "api.internal".into(),
            port: 8080,
        });
        assert_eq!(host.name(), "dns:api.internal:8080");
        let srv = DnsSource::new(DnsQuery::Srv {
            name: "_http._tcp.api.internal".into(),
        });
        assert_eq!(srv.name(), "srv:_http._tcp.api.internal");
    }
}
