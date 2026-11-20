//! Service discovery: where a cluster's endpoints come from when they are
//! not listed in the file.
//!
//! A [`Source`] (DNS, a watched file, Kubernetes EndpointSlices, a delta
//! feed) produces [`Update`]s: a whole endpoint set, a change to it, or a
//! failure. A cluster may have several sources; its [`Merger`] keeps each
//! source's set apart and merges them into the endpoint set the balancer
//! sees, emitting a [`Change`] (what was added, removed or changed, and a
//! version) only when the merged set actually moves.
//!
//! # Merge rules
//!
//! 1. **Union.** The merged set is every address any source has. An
//!    address two sources both name appears once.
//! 2. **First source wins.** For an address several sources name, the
//!    weight, priority, zone and readiness come from the source listed
//!    first in the cluster.
//! 3. **Last known good.** A source that fails keeps the set it last
//!    produced successfully, and the failure is counted in its
//!    [`SourceStatus`] (and so in metrics). It is marked stale until its
//!    next success. A failure never removes an endpoint.
//! 4. **Nothing before the first success.** A source that has never
//!    succeeded contributes nothing.
//! 5. **Empty is an answer.** A successful update with no endpoints
//!    empties that source: an endpoint removed from DNS must stop taking
//!    traffic. Only a failure keeps the old set.
//! 6. **Not ready is kept, not used.** An endpoint a source reports as not
//!    ready stays in the merged set with `ready: false`, so the balancer
//!    keeps its state but sends it nothing, and a readiness flip is a
//!    change, not a removal and an add.
//! 7. **Deltas apply in order.** An [`UpdateKind::Upsert`] or
//!    [`UpdateKind::Remove`] changes one source's set; one for a source
//!    that has not yet sent a full set starts it from empty.

pub mod delta;
pub mod dns;
pub mod file;
pub mod k8s;

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

/// One discovered endpoint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Endpoint {
    /// `host:port`.
    pub address: String,
    pub weight: u32,
    pub priority: u8,
    /// The zone it runs in, when the source knows (Kubernetes
    /// `topology.kubernetes.io/zone`, say).
    pub zone: Option<String>,
    /// Whether it may take traffic (Kubernetes readiness, say).
    pub ready: bool,
}

impl Endpoint {
    /// A ready endpoint with weight 1 and priority 0.
    pub fn new(address: impl Into<String>) -> Endpoint {
        Endpoint {
            address: address.into(),
            weight: 1,
            priority: 0,
            zone: None,
            ready: true,
        }
    }
}

/// The source a cluster's configuration names, with `dns_servers` for
/// the DNS ones (the system's when empty).
pub fn from_config(
    source: &edgefront_core::config::DiscoverySource,
    dns_servers: &[std::net::SocketAddr],
) -> Box<dyn Source> {
    use edgefront_core::config::DiscoverySource as Config;
    match source {
        Config::Dns(target) => {
            // Validation checked it is host:port with a nonzero port.
            let (name, port) = target.rsplit_once(':').unwrap_or((target, "0"));
            let query = dns::DnsQuery::Host {
                name: name.to_owned(),
                port: port.parse().unwrap_or(0),
            };
            Box::new(dns::DnsSource::new(query).servers(dns_servers.to_vec()))
        }
        Config::Srv(name) => {
            let query = dns::DnsQuery::Srv { name: name.clone() };
            Box::new(dns::DnsSource::new(query).servers(dns_servers.to_vec()))
        }
        Config::File(path) => Box::new(file::FileSource::new(path)),
        Config::Delta(d) => {
            let mut source = delta::DeltaSource::new(&d.url);
            source.ca_file = d.ca_file.as_ref().map(std::path::PathBuf::from);
            source.token_file = d.token_file.as_ref().map(std::path::PathBuf::from);
            Box::new(source)
        }
        Config::Kubernetes(k) => {
            let source = k8s::KubernetesSource::new(&k.namespace, &k.service, k.port.as_deref());
            Box::new(match &k.api_server {
                Some(server) => {
                    let path = |p: &Option<String>| p.as_ref().map(std::path::PathBuf::from);
                    let mut api = k8s::ApiAccess::new(server);
                    api.ca_file = path(&k.ca_file);
                    api.token_file = path(&k.token_file);
                    api.cert_file = path(&k.cert_file);
                    api.key_file = path(&k.key_file);
                    source.api(api)
                }
                None => source,
            })
        }
    }
}

/// What a source reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateKind {
    /// The source's whole set now.
    Set(Vec<Endpoint>),
    /// One endpoint added or changed.
    Upsert(Endpoint),
    /// One endpoint gone, by address.
    Remove(String),
    /// The source could not be read; its last set stays.
    Failed(String),
}

/// An update from the source at `source` (its index in the cluster's
/// list of sources).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub source: usize,
    pub kind: UpdateKind,
}

/// Something that discovers endpoints. A source runs on its own task and
/// sends [`UpdateKind`]s down `tx` until it is told to stop or `tx` is
/// closed.
pub trait Source: Send + 'static {
    /// A short name for logs and metrics, such as `dns:api.internal`.
    fn name(&self) -> String;

    /// Runs the source, sending its updates on `tx`.
    fn run(
        self: Box<Self>,
        tx: tokio::sync::mpsc::Sender<UpdateKind>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
}

/// How one source is doing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceStatus {
    pub name: String,
    /// Successful updates applied.
    pub updates: u64,
    /// Failures reported.
    pub failures: u64,
    /// The last failure's message, until the next success.
    pub last_error: Option<String>,
    /// The source failed after its last success, so its set is last known
    /// good rather than current.
    pub stale: bool,
    /// Whether it has ever succeeded.
    pub synced: bool,
    /// When it last succeeded.
    pub last_success: Option<Instant>,
}

/// What a merge changed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Change {
    /// The merged set's version after the change; it rises by one with
    /// every change.
    pub version: u64,
    pub added: Vec<Endpoint>,
    pub removed: Vec<String>,
    /// Endpoints whose weight, priority, zone or readiness changed, as
    /// they are now.
    pub changed: Vec<Endpoint>,
}

impl Change {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// A cluster's sources merged into one endpoint set.
#[derive(Debug)]
pub struct Merger {
    sources: Vec<(SourceStatus, HashMap<String, Endpoint>)>,
    merged: BTreeMap<String, Endpoint>,
    version: u64,
}

impl Merger {
    /// A merger for sources with these names, in the cluster's order.
    pub fn new(names: impl IntoIterator<Item = String>) -> Merger {
        Merger {
            sources: names
                .into_iter()
                .map(|name| {
                    (
                        SourceStatus {
                            name,
                            ..SourceStatus::default()
                        },
                        HashMap::new(),
                    )
                })
                .collect(),
            merged: BTreeMap::new(),
            version: 0,
        }
    }

    /// The merged set, ordered by address.
    pub fn endpoints(&self) -> impl Iterator<Item = &Endpoint> {
        self.merged.values()
    }

    /// The merged set's version.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Each source's status, in order.
    pub fn status(&self) -> impl Iterator<Item = &SourceStatus> {
        self.sources.iter().map(|(s, _)| s)
    }

    /// Applies `update`, returning what changed in the merged set (empty
    /// when nothing did). An update for a source the merger does not have
    /// changes nothing.
    pub fn apply(&mut self, update: Update, now: Instant) -> Change {
        let Some((status, set)) = self.sources.get_mut(update.source) else {
            return Change::default();
        };
        match update.kind {
            UpdateKind::Failed(e) => {
                status.failures += 1;
                status.last_error = Some(e);
                status.stale = status.synced;
                Change::default()
            }
            UpdateKind::Set(list) => {
                let mut touched: Vec<String> = set.keys().cloned().collect();
                set.clear();
                for e in list {
                    if !set.contains_key(&e.address) {
                        touched.push(e.address.clone());
                    }
                    set.insert(e.address.clone(), e);
                }
                touched.sort_unstable();
                touched.dedup();
                status.updates += 1;
                Merger::succeeded(status, now);
                self.remerge(touched.iter().map(String::as_str))
            }
            UpdateKind::Upsert(e) => {
                let address = e.address.clone();
                set.insert(e.address.clone(), e);
                status.updates += 1;
                Merger::succeeded(status, now);
                self.remerge([address.as_str()])
            }
            UpdateKind::Remove(address) => {
                set.remove(&address);
                status.updates += 1;
                Merger::succeeded(status, now);
                self.remerge([address.as_str()])
            }
        }
    }

    fn succeeded(status: &mut SourceStatus, now: Instant) {
        status.synced = true;
        status.stale = false;
        status.last_error = None;
        status.last_success = Some(now);
    }

    /// Settles the merged entry of each of `addresses`, the only ones an
    /// update touched, so an update costs its own size rather than the
    /// whole set's: the first source (in order) naming an address wins it.
    fn remerge<'a>(&mut self, addresses: impl IntoIterator<Item = &'a str>) -> Change {
        let mut change = Change::default();
        for address in addresses {
            let winner = self.sources.iter().find_map(|(_, set)| set.get(address));
            match (winner, self.merged.get(address)) {
                (None, None) => {}
                (None, Some(_)) => {
                    self.merged.remove(address);
                    change.removed.push(address.to_owned());
                }
                (Some(e), None) => {
                    change.added.push(e.clone());
                    self.merged.insert(address.to_owned(), e.clone());
                }
                (Some(e), Some(old)) if old != e => {
                    change.changed.push(e.clone());
                    self.merged.insert(address.to_owned(), e.clone());
                }
                (Some(_), Some(_)) => {}
            }
        }
        if !change.is_empty() {
            self.version += 1;
        }
        change.version = self.version;
        change
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(address: &str) -> Endpoint {
        Endpoint::new(address)
    }

    fn set(source: usize, list: &[Endpoint]) -> Update {
        Update {
            source,
            kind: UpdateKind::Set(list.to_vec()),
        }
    }

    fn addresses(m: &Merger) -> Vec<&str> {
        m.endpoints().map(|e| e.address.as_str()).collect()
    }

    fn merger(n: usize) -> Merger {
        Merger::new((0..n).map(|i| format!("s{i}")))
    }

    #[test]
    fn sources_are_unioned_once_per_address() {
        let mut m = merger(2);
        let now = Instant::now();
        let c = m.apply(set(0, &[ep("a:1"), ep("b:1")]), now);
        assert_eq!(c.added.len(), 2);
        assert_eq!(c.version, 1);
        let c = m.apply(set(1, &[ep("b:1"), ep("c:1")]), now);
        assert_eq!(c.added, vec![ep("c:1")]);
        assert_eq!(addresses(&m), ["a:1", "b:1", "c:1"]);
        assert_eq!(m.version(), 2);
    }

    #[test]
    fn the_first_source_listed_wins_an_address() {
        let mut m = merger(2);
        let now = Instant::now();
        let heavy = Endpoint {
            weight: 9,
            ..ep("b:1")
        };
        m.apply(set(1, std::slice::from_ref(&heavy)), now);
        assert_eq!(m.endpoints().next().unwrap().weight, 9);
        // Source 0 is listed first: its view of b:1 wins.
        let c = m.apply(set(0, &[ep("b:1")]), now);
        assert_eq!(c.changed, vec![ep("b:1")]);
        assert_eq!(m.endpoints().next().unwrap().weight, 1);
        // And when source 0 lets go, source 1's view comes back.
        let c = m.apply(set(0, &[]), now);
        assert_eq!(c.changed, vec![heavy]);
        assert!(c.removed.is_empty());
    }

    #[test]
    fn a_failure_keeps_the_last_known_good_set() {
        let mut m = merger(1);
        let now = Instant::now();
        m.apply(set(0, &[ep("a:1")]), now);
        let c = m.apply(
            Update {
                source: 0,
                kind: UpdateKind::Failed("SERVFAIL".into()),
            },
            now,
        );
        assert!(c.is_empty());
        assert_eq!(addresses(&m), ["a:1"]);
        let s = m.status().next().unwrap();
        assert_eq!((s.failures, s.stale, s.synced), (1, true, true));
        assert_eq!(s.last_error.as_deref(), Some("SERVFAIL"));
        // The next success clears the error and the staleness.
        m.apply(set(0, &[ep("a:1")]), now);
        let s = m.status().next().unwrap();
        assert_eq!((s.failures, s.stale, s.updates), (1, false, 2));
        assert_eq!(s.last_error, None);
        assert_eq!(s.last_success, Some(now));
    }

    #[test]
    fn a_source_that_never_succeeded_contributes_nothing() {
        let mut m = merger(2);
        let now = Instant::now();
        m.apply(
            Update {
                source: 1,
                kind: UpdateKind::Failed("connection refused".into()),
            },
            now,
        );
        let s = m.status().nth(1).unwrap();
        assert!(!s.synced);
        assert!(!s.stale, "nothing to be stale about");
        m.apply(set(0, &[ep("a:1")]), now);
        assert_eq!(addresses(&m), ["a:1"]);
    }

    #[test]
    fn an_empty_answer_empties_the_source() {
        let mut m = merger(1);
        let now = Instant::now();
        m.apply(set(0, &[ep("a:1"), ep("b:1")]), now);
        let c = m.apply(set(0, &[]), now);
        assert_eq!(c.removed, ["a:1", "b:1"]);
        assert_eq!(m.endpoints().count(), 0);
    }

    #[test]
    fn readiness_flips_are_changes_not_removals() {
        let mut m = merger(1);
        let now = Instant::now();
        m.apply(set(0, &[ep("a:1")]), now);
        let down = Endpoint {
            ready: false,
            ..ep("a:1")
        };
        let c = m.apply(set(0, std::slice::from_ref(&down)), now);
        assert_eq!(c.changed, vec![down]);
        assert!(c.added.is_empty() && c.removed.is_empty());
        assert!(!m.endpoints().next().unwrap().ready);
    }

    #[test]
    fn deltas_apply_in_order_and_start_from_empty() {
        let mut m = merger(1);
        let now = Instant::now();
        let c = m.apply(
            Update {
                source: 0,
                kind: UpdateKind::Upsert(ep("a:1")),
            },
            now,
        );
        assert_eq!(c.added, vec![ep("a:1")]);
        m.apply(
            Update {
                source: 0,
                kind: UpdateKind::Upsert(ep("b:1")),
            },
            now,
        );
        let c = m.apply(
            Update {
                source: 0,
                kind: UpdateKind::Remove("a:1".into()),
            },
            now,
        );
        assert_eq!(c.removed, ["a:1"]);
        assert_eq!(addresses(&m), ["b:1"]);
        // Removing what is not there changes nothing and keeps the version.
        let v = m.version();
        let c = m.apply(
            Update {
                source: 0,
                kind: UpdateKind::Remove("zz:1".into()),
            },
            now,
        );
        assert!(c.is_empty());
        assert_eq!(c.version, v);
    }

    #[test]
    fn unknown_sources_and_repeats_change_nothing() {
        let mut m = merger(1);
        let now = Instant::now();
        assert!(m.apply(set(5, &[ep("a:1")]), now).is_empty());
        m.apply(set(0, &[ep("a:1")]), now);
        let v = m.version();
        assert!(m.apply(set(0, &[ep("a:1")]), now).is_empty());
        assert_eq!(m.version(), v);
    }
}
