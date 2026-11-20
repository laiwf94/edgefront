//! Tiered caching across a cluster.
//!
//! - [`owner`]: which node owns a key, by weighted rendezvous hashing;
//! - [`peer`]: the signed hop header between nodes, so a node that does not
//!   own a key fetches it from the owner, once, and never further;
//! - [`shield`]: per route, the upper-tier nodes misses go to instead of the
//!   origin;
//! - [`hot`]: a count-min sketch of this node's requests, so a key requested
//!   often enough is kept here instead of fetched from its owner each time,
//!   and an owner that sees one of its keys get hot has its two runners-up
//!   fetch and keep it too ([`REPLICAS`] in all).

pub mod hot;
pub mod owner;
pub mod peer;
pub mod shield;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use edgefront_core::cluster::swim::Member;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use hot::{Change, HotKeys};
use owner::{Node, Owners};
use peer::{Hop, Tier as HopTier};
use shield::Shields;

/// The nodes that hold a hot key whatever traffic they see: its owner and
/// its runners-up, which take it over if the owner leaves.
pub const REPLICAS: usize = 3;

/// A response extension: fetched from the key's owner for a key that is not
/// hot here, so this node passes it on without storing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transient;

/// What a node announces about itself in its SWIM metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    /// Where its HTTP listener is reached.
    pub http: SocketAddr,
    /// Its share of the keys.
    pub weight: f64,
}

impl Meta {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("meta serializes")
    }

    pub fn parse(text: &str) -> Option<Meta> {
        serde_json::from_str(text).ok()
    }
}

/// How this node takes part in the tiers.
#[derive(Debug, Clone)]
pub struct TierSettings {
    /// This node's id, as in its SWIM membership.
    pub me: String,
    /// The cluster key that signs hops.
    pub key: Vec<u8>,
    pub shields: Shields,
    /// A key requested this often a second on one node is kept there.
    pub hot_rate: f64,
    /// The window the hot-key sketch halves over.
    pub hot_window: Duration,
}

/// The live members as the tiers see them.
#[derive(Debug, Default)]
pub struct View {
    /// The edge nodes (every live member that is not a shield), by weight.
    pub edges: Owners<SocketAddr>,
    /// Every live member's HTTP address, shields included.
    pub http: HashMap<String, SocketAddr>,
}

impl View {
    pub fn new(members: &[Member], shields: &Shields) -> View {
        let mut http = HashMap::new();
        let mut edges = Vec::new();
        for m in members {
            let Some(meta) = Meta::parse(&m.meta) else {
                continue;
            };
            http.insert(m.id.clone(), meta.http);
            if !shields.is_shield(&m.id) {
                edges.push(Node {
                    id: m.id.clone(),
                    weight: meta.weight,
                    value: meta.http,
                });
            }
        }
        View {
            edges: Owners::new(edges),
            http,
        }
    }
}

/// Where a miss goes next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    Origin,
    /// Another node: the key's owner, or a shield.
    Node {
        id: String,
        addr: SocketAddr,
        tier: HopTier,
    },
}

/// Counters for the node's metrics.
#[derive(Debug, Default)]
pub struct TierStats {
    /// Misses sent to the key's owner.
    pub owner_fetches: AtomicU64,
    /// Misses sent to a shield.
    pub shield_fetches: AtomicU64,
    /// Node fetches that failed and went to the origin instead.
    pub fallbacks: AtomicU64,
    pub hot_promoted: AtomicU64,
    pub hot_demoted: AtomicU64,
    /// Hot keys this node owns pushed to a runner-up.
    pub replicas_sent: AtomicU64,
}

impl TierStats {
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

/// A node's part in the tiers: the membership view and the hot keys.
pub struct Tier {
    pub settings: TierSettings,
    members: watch::Receiver<Vec<Member>>,
    view: Mutex<Arc<View>>,
    hot: Mutex<HotKeys>,
    started: Instant,
    pub stats: TierStats,
}

impl Tier {
    pub fn new(settings: TierSettings, members: watch::Receiver<Vec<Member>>) -> Tier {
        let view = View::new(&members.borrow(), &settings.shields);
        let hot = HotKeys::new(settings.hot_rate, settings.hot_window);
        Tier {
            settings,
            members,
            view: Mutex::new(Arc::new(view)),
            hot: Mutex::new(hot),
            started: Instant::now(),
            stats: TierStats::default(),
        }
    }

    /// The current view, rebuilt when the membership has changed.
    pub fn view(&self) -> Arc<View> {
        let mut view = self.view.lock().unwrap_or_else(PoisonError::into_inner);
        if self.members.has_changed().unwrap_or(false) {
            let mut rx = self.members.clone();
            let members = rx.borrow_and_update().clone();
            *view = Arc::new(View::new(&members, &self.settings.shields));
        }
        view.clone()
    }

    /// Whether this node is a shield.
    pub fn is_shield(&self) -> bool {
        self.settings.shields.is_shield(&self.settings.me)
    }

    /// Whether this node owns `key` among the live edge nodes. A shield, or
    /// a node that sees no edges, counts as owning everything it is asked.
    pub fn owns(&self, key: &str) -> bool {
        let view = self.view();
        self.is_shield()
            || view
                .edges
                .owner(key)
                .is_none_or(|o| o.id == self.settings.me)
    }

    /// Counts a request for `key`: whether it just became hot here, and the
    /// keys the sketch demoted since the last call.
    pub fn observe(&self, key: &str) -> (bool, Vec<String>) {
        let now = self.started.elapsed().as_millis() as u64;
        let mut hot = self.hot.lock().unwrap_or_else(PoisonError::into_inner);
        let demoted = hot.age(now);
        let promoted = hot.observe(key, now) == Change::Promoted;
        if promoted {
            self.stats.hot_promoted.fetch_add(1, Ordering::Relaxed);
        }
        self.stats
            .hot_demoted
            .fetch_add(demoted.len() as u64, Ordering::Relaxed);
        (promoted, demoted)
    }

    /// The runners-up a hot `key` this node owns is pushed to: the rest of
    /// its [`REPLICAS`] highest-scoring edge nodes. Empty when this node
    /// does not own it.
    pub fn replicas(&self, key: &str) -> Vec<(String, SocketAddr)> {
        let view = self.view();
        let top = view.edges.top(key, REPLICAS);
        if top.first().is_none_or(|o| o.id != self.settings.me) {
            return Vec::new();
        }
        top[1..].iter().map(|n| (n.id.clone(), n.value)).collect()
    }

    /// Whether this node keeps `key` whatever its own traffic: it is the
    /// owner or one of the runners-up.
    pub fn keeps(&self, key: &str) -> bool {
        self.is_shield()
            || self
                .view()
                .edges
                .top(key, REPLICAS)
                .iter()
                .any(|n| n.id == self.settings.me)
            || self.view().edges.is_empty()
    }

    /// Whether `key` is hot on this node.
    pub fn is_hot(&self, key: &str) -> bool {
        self.hot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_hot(key)
    }

    /// Where a miss for `key` (a request to `host` and `path`) goes: to its
    /// owner when another edge node owns it and the request has made no hop
    /// yet; to the route's shield when there is one and this node is not a
    /// shield; otherwise to the origin.
    pub fn next(&self, key: &str, host: &str, path: &str, hop: Option<&Hop>) -> Next {
        let view = self.view();
        let me = &self.settings.me;
        if self.is_shield() {
            return Next::Origin;
        }
        if hop.is_none()
            && let Some(owner) = view.edges.owner(key)
            && owner.id != *me
        {
            return Next::Node {
                id: owner.id.clone(),
                addr: owner.value,
                tier: HopTier::Owner,
            };
        }
        if hop.is_some_and(|h| h.tier == HopTier::Shield) {
            return Next::Origin;
        }
        // A replica fetches from the owner that told it to.
        if hop.is_some_and(|h| h.tier == HopTier::Replica)
            && let Some(owner) = view.edges.owner(key)
            && owner.id != *me
        {
            return Next::Node {
                id: owner.id.clone(),
                addr: owner.value,
                tier: HopTier::Owner,
            };
        }
        if let Some(set) = self.settings.shields.for_request(host, path) {
            let alive: Vec<Node<SocketAddr>> = set
                .iter()
                .filter_map(|id| {
                    view.http.get(id).map(|addr| Node {
                        id: id.clone(),
                        weight: 1.0,
                        value: *addr,
                    })
                })
                .collect();
            if let Some(s) = Owners::new(alive).owner(key) {
                return Next::Node {
                    id: s.id.clone(),
                    addr: s.value,
                    tier: HopTier::Shield,
                };
            }
        }
        Next::Origin
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgefront_core::cluster::swim::State;
    use shield::ShieldRoute;

    fn member(id: &str, port: u16) -> Member {
        Member {
            id: id.into(),
            addr: SocketAddr::from(([127, 0, 0, 1], port + 1000)),
            meta: Meta {
                http: SocketAddr::from(([127, 0, 0, 1], port)),
                weight: 1.0,
            }
            .to_json(),
            state: State::Alive,
            incarnation: 0,
        }
    }

    fn tier(me: &str, members: Vec<Member>) -> (Tier, watch::Sender<Vec<Member>>) {
        let (tx, rx) = watch::channel(members);
        let shields = Shields::new(vec![ShieldRoute {
            host: String::new(),
            path_prefix: "/shielded/".into(),
            nodes: vec!["shield".into()],
        }]);
        let t = Tier::new(
            TierSettings {
                me: me.into(),
                key: b"k".to_vec(),
                shields,
                hot_rate: 5.0,
                hot_window: Duration::from_secs(1),
            },
            rx,
        );
        (t, tx)
    }

    fn cluster() -> Vec<Member> {
        let mut m: Vec<Member> = (0..4).map(|i| member(&format!("e{i}"), 8000 + i)).collect();
        m.push(member("shield", 9000));
        let mut bad = member("no-meta", 1);
        bad.meta = "not json".into();
        m.push(bad);
        m
    }

    #[test]
    fn misses_go_to_the_owner_then_the_shield_then_the_origin() {
        let (t, _tx) = tier("e0", cluster());
        let view = t.view();
        assert_eq!(
            view.edges.len(),
            4,
            "the shield and the node without meta own nothing"
        );
        assert_eq!(view.http.len(), 5);
        let key = (0..100)
            .map(|i| format!("example.com/shielded/{i}"))
            .find(|k| view.edges.owner(k).unwrap().id != "e0")
            .unwrap();
        let owner = view.edges.owner(&key).unwrap().clone();
        assert!(!t.owns(&key));
        assert_eq!(
            t.next(&key, "example.com", "/shielded/x", None),
            Next::Node {
                id: owner.id.clone(),
                addr: owner.value,
                tier: HopTier::Owner
            }
        );
        // From another edge: this node is the owner's stand-in, so on to the
        // route's shield, and never to another peer.
        let from_edge = Hop {
            from: "e1".into(),
            tier: HopTier::Owner,
        };
        assert_eq!(
            t.next(&key, "example.com", "/shielded/x", Some(&from_edge)),
            Next::Node {
                id: "shield".into(),
                addr: SocketAddr::from(([127, 0, 0, 1], 9000)),
                tier: HopTier::Shield
            }
        );
        assert_eq!(
            t.next(&key, "example.com", "/other", Some(&from_edge)),
            Next::Origin
        );
        let from_shield_hop = Hop {
            from: "e1".into(),
            tier: HopTier::Shield,
        };
        assert_eq!(
            t.next(&key, "example.com", "/shielded/x", Some(&from_shield_hop)),
            Next::Origin
        );
        // A key this node owns goes to the shield, or the origin.
        let mine = (0..100)
            .map(|i| format!("example.com/shielded/{i}"))
            .find(|k| view.edges.owner(k).unwrap().id == "e0")
            .unwrap();
        assert!(t.owns(&mine));
        assert!(matches!(
            t.next(&mine, "example.com", "/shielded/a", None),
            Next::Node {
                tier: HopTier::Shield,
                ..
            }
        ));
        assert_eq!(t.next(&mine, "example.com", "/plain", None), Next::Origin);
        // A shield always goes to the origin and owns what it is asked.
        let (s, _tx) = tier("shield", cluster());
        assert!(s.is_shield() && s.owns(&key));
        assert_eq!(
            s.next(&key, "example.com", "/shielded/x", None),
            Next::Origin
        );
    }

    #[test]
    fn the_view_follows_the_membership() {
        let (t, tx) = tier("e0", cluster());
        let key = (0..100)
            .map(|i| format!("h/{i}"))
            .find(|k| t.view().edges.owner(k).unwrap().id == "e3")
            .unwrap();
        tx.send(cluster().into_iter().filter(|m| m.id != "e3").collect())
            .unwrap();
        assert_eq!(t.view().edges.len(), 3);
        assert_ne!(t.view().edges.owner(&key).unwrap().id, "e3");
        // The shield gone: shielded misses go to the origin.
        tx.send(vec![member("e0", 8000)]).unwrap();
        assert!(t.owns(&key));
        assert_eq!(t.next(&key, "h", "/shielded/1", None), Next::Origin);
        // Alone with no edges at all (not even itself announced).
        tx.send(Vec::new()).unwrap();
        assert!(t.owns(&key));
    }

    #[test]
    fn hot_keys_are_counted_and_meta_round_trips() {
        let (t, _tx) = tier("e0", cluster());
        let promoted = (0..20).filter(|_| t.observe("popular").0).count();
        assert_eq!(promoted, 1, "promoted once");
        assert!(t.is_hot("popular"));
        assert_eq!(TierStats::get(&t.stats.hot_promoted), 1);
        assert!(!t.observe("rare").0);
        // The replicas of a key this node owns: its two runners-up.
        let view = t.view();
        let mine = (0..100)
            .map(|i| format!("h/{i}"))
            .find(|k| view.edges.owner(k).unwrap().id == "e0")
            .unwrap();
        let reps = t.replicas(&mine);
        let top: Vec<String> = view
            .edges
            .top(&mine, 3)
            .iter()
            .map(|n| n.id.clone())
            .collect();
        assert_eq!(
            reps.iter().map(|r| r.0.clone()).collect::<Vec<_>>(),
            top[1..]
        );
        assert!(t.keeps(&mine));
        let theirs = (0..100)
            .map(|i| format!("h/{i}"))
            .find(|k| !view.edges.top(k, 3).iter().any(|n| n.id == "e0"))
            .unwrap();
        assert!(t.replicas(&theirs).is_empty() && !t.keeps(&theirs));
        let m = Meta {
            http: "10.0.0.1:80".parse().unwrap(),
            weight: 2.5,
        };
        assert_eq!(Meta::parse(&m.to_json()), Some(m));
        assert_eq!(Meta::parse("{}"), None);
    }
}
