//! Tiered caching across a cluster in one process: ten edge nodes and a
//! shield over loopback, joined by SWIM, each a whole node with its own
//! listeners, in front of one counting origin.
mod common;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use cachet::node::{self, Listeners, Node};
use cachet::origin::Client;
use common::{TestOrigin, handler, http_on, node_config, reply, secrets};
use edgefront_cache::tier::TierStats;
use tokio::net::{TcpListener, UdpSocket};

/// An origin that answers every path with a cacheable body naming it.
async fn origin() -> TestOrigin {
    TestOrigin::start(handler(|parts, _| {
        let body = format!("body of {}", parts.uri.path());
        reply(200, &[("cache-control", "max-age=600")], body)
    }))
    .await
}

struct Cluster {
    nodes: Vec<Option<Node>>,
    ids: Vec<String>,
    client: Client,
}

impl Cluster {
    /// `edges` edge nodes, plus a shield named `shield` for every path when
    /// `shield` is set, all joined through the first node.
    async fn start(origin: SocketAddr, edges: usize, shield: bool, extra: &str) -> Cluster {
        let mut ids: Vec<String> = (0..edges).map(|i| format!("edge-{i}")).collect();
        if shield {
            ids.push("shield".into());
        }
        let mut sockets = Vec::new();
        for _ in &ids {
            sockets.push((
                TcpListener::bind("127.0.0.1:0").await.unwrap(),
                TcpListener::bind("127.0.0.1:0").await.unwrap(),
                UdpSocket::bind("127.0.0.1:0").await.unwrap(),
            ));
        }
        let seed = sockets[0].2.local_addr().unwrap();
        let shields = if shield {
            "[[tier.shields]]\npath_prefix = \"/\"\nnodes = [\"shield\"]\n"
        } else {
            ""
        };
        let mut nodes = Vec::new();
        for (id, (proxy, admin, gossip)) in ids.iter().zip(sockets) {
            let text = format!(
                "[tier]\ngossip_listen = \"127.0.0.1:0\"\nseeds = [\"{seed}\"]\n{extra}{shields}"
            );
            let config = node_config(id, &[origin], &text);
            let listeners = Listeners {
                proxy,
                admin,
                gossip: Some(gossip),
            };
            nodes.push(Some(
                node::start_with(&config, &secrets(), listeners)
                    .await
                    .unwrap(),
            ));
        }
        let c = Cluster {
            nodes,
            ids,
            client: cachet::origin::client(Duration::from_secs(2)),
        };
        c.converge(c.ids.len(), Duration::from_secs(10)).await;
        c
    }

    fn live(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().flatten()
    }

    /// Waits until every live node sees `n` live members.
    async fn converge(&self, n: usize, limit: Duration) -> Duration {
        let start = Instant::now();
        loop {
            if self
                .live()
                .all(|node| node.proxy.tier().unwrap().view().http.len() == n)
            {
                return start.elapsed();
            }
            assert!(start.elapsed() < limit, "membership did not settle on {n}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn get(&self, node: usize, path: &str) -> String {
        let addr = self.nodes[node].as_ref().unwrap().proxy_addr;
        let got = http_on(
            &self.client,
            "GET",
            addr,
            path,
            &[("host", "example.com")],
            "",
        )
        .await;
        assert_eq!(got.status, 200, "{path} on node {node}");
        String::from_utf8(got.body.to_vec()).unwrap()
    }

    fn stat(
        &self,
        f: impl Fn(&edgefront_cache::tier::TierStats) -> &std::sync::atomic::AtomicU64,
    ) -> u64 {
        self.live()
            .map(|n| TierStats::get(f(&n.proxy.tier().unwrap().stats)))
            .sum()
    }

    async fn shutdown(self) {
        for n in self.nodes.into_iter().flatten() {
            n.shutdown().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stampede_on_ten_nodes_and_a_shield_is_one_origin_request() {
    let o = origin().await;
    o.set_delay(Duration::from_millis(200));
    let c = Cluster::start(o.addr, 10, true, "").await;
    // Every edge node asked for the same cold object at once, ten times.
    let mut tasks = Vec::new();
    for round in 0..10 {
        for n in 0..10 {
            let addr = c.nodes[n].as_ref().unwrap().proxy_addr;
            let client = c.client.clone();
            tasks.push(tokio::spawn(async move {
                let _ = round;
                http_on(
                    &client,
                    "GET",
                    addr,
                    "/cold.js",
                    &[("host", "example.com")],
                    "",
                )
                .await
            }));
        }
    }
    for t in tasks {
        let got = t.await.unwrap();
        assert_eq!(got.status, 200);
        assert_eq!(got.body, "body of /cold.js");
    }
    assert_eq!(
        o.hits(),
        1,
        "{:?}",
        o.seen().iter().map(|s| s.uri.clone()).collect::<Vec<_>>()
    );
    // The owner fetched it from the shield; the others from the owner.
    assert_eq!(c.stat(|s| &s.shield_fetches), 1);
    assert!(c.stat(|s| &s.owner_fetches) >= 9);
    assert_eq!(c.stat(|s| &s.fallbacks), 0);
    c.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_killed_node_is_noticed_within_three_seconds_and_only_its_keys_move() {
    let o = origin().await;
    let mut c = Cluster::start(o.addr, 10, false, "").await;
    let keys: Vec<String> = (0..10_000).map(|i| format!("example.com/k/{i}")).collect();
    let owner_of = |c: &Cluster, from: usize| -> Vec<String> {
        let view = c.nodes[from].as_ref().unwrap().proxy.tier().unwrap().view();
        keys.iter()
            .map(|k| view.edges.owner(k).unwrap().id.clone())
            .collect()
    };
    let before = owner_of(&c, 0);
    let killed = Instant::now();
    c.nodes[7].take().unwrap().abort();
    let took = c.converge(9, Duration::from_secs(3)).await;
    let detected = killed.elapsed();
    let after = owner_of(&c, 0);
    let mut moved = 0;
    for (b, a) in before.iter().zip(&after) {
        if b != a {
            assert_eq!(b, "edge-7", "a key moved from a live node");
            moved += 1;
        }
    }
    println!(
        "killed node gone everywhere after {detected:?} ({took:?} of waiting); {moved} of 10,000 keys moved"
    );
    assert!(detected <= Duration::from_secs(3));
    assert!(moved < 1_100, "{moved}");
    // Its keys are served by their new owners.
    let k = keys
        .iter()
        .zip(&before)
        .find(|(_, o)| *o == "edge-7")
        .unwrap()
        .0;
    let path = k.strip_prefix("example.com").unwrap();
    assert_eq!(c.get(0, path).await, format!("body of {path}"));
    c.shutdown().await;
}

/// Zipf(1.0) ranks over `n` keys, drawn with a fixed seed.
fn zipf(n: usize, draws: usize, seed: u64) -> Vec<usize> {
    let weights: Vec<f64> = (1..=n).map(|r| 1.0 / r as f64).collect();
    let total: f64 = weights.iter().sum();
    let mut cdf = Vec::with_capacity(n);
    let mut acc = 0.0;
    for w in &weights {
        acc += w / total;
        cdf.push(acc);
    }
    let mut x = seed;
    (0..draws)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let u = (x >> 11) as f64 / (1u64 << 53) as f64;
            cdf.partition_point(|c| *c < u).min(n - 1)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn origin_offload_on_a_zipf_trace_across_ten_nodes() {
    const KEYS: usize = 1_000;
    const REQUESTS: usize = 40_000;
    let trace = zipf(KEYS, REQUESTS, 7);
    let distinct = trace
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    // Each request lands on a node chosen at random, as behind a load
    // balancer.
    let landing: Vec<usize> = (0..REQUESTS).map(|i| (i * 7 + i / 10) % 10).collect();
    let run = |tiers: bool| {
        let trace = trace.clone();
        let landing = landing.clone();
        async move {
            let o = origin().await;
            let c = if tiers {
                Cluster::start(o.addr, 10, false, "").await
            } else {
                // Ten nodes that do not know each other.
                let mut nodes = Vec::new();
                for i in 0..10 {
                    let config = node_config(&format!("alone-{i}"), &[o.addr], "");
                    nodes.push(Some(node::start(&config, &secrets()).await.unwrap()));
                }
                Cluster {
                    nodes,
                    ids: Vec::new(),
                    client: cachet::origin::client(Duration::from_secs(2)),
                }
            };
            for chunk in trace.chunks(200).zip(landing.chunks(200)) {
                let mut tasks = Vec::new();
                for (key, node) in chunk.0.iter().zip(chunk.1) {
                    let addr = c.nodes[*node].as_ref().unwrap().proxy_addr;
                    let client = c.client.clone();
                    let path = format!("/obj/{key}");
                    tasks.push(tokio::spawn(async move {
                        http_on(&client, "GET", addr, &path, &[("host", "example.com")], "").await
                    }));
                }
                for t in tasks {
                    assert_eq!(t.await.unwrap().status, 200);
                }
            }
            let hits = o.hits();
            c.shutdown().await;
            hits
        }
    };
    let alone = run(false).await;
    let tiered = run(true).await;
    let offload = |origin: usize| 1.0 - origin as f64 / REQUESTS as f64;
    // One node that saw every request would go to the origin once per
    // distinct key: the single-node baseline.
    println!(
        "{REQUESTS} requests for {distinct} distinct keys of {KEYS}: one node alone would offload {:.1}%; ten separate nodes sent {alone} to the origin ({:.1}%), ten tiered nodes {tiered} ({:.1}%)",
        offload(distinct) * 100.0,
        offload(alone) * 100.0,
        offload(tiered) * 100.0
    );
    assert!(offload(tiered) >= 0.95, "{tiered}");
    assert!(tiered < alone);
    // The tiers send each object to the origin once.
    assert_eq!(tiered, distinct);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hot_keys_are_kept_on_every_node_that_sees_them_hot() {
    let o = origin().await;
    // Hot at 5 requests a second on a node.
    let c = Cluster::start(o.addr, 10, false, "hot_rate = 5\n").await;
    let top: Vec<String> = (0..100).map(|i| format!("/top/{i}")).collect();
    // Warm-up: the top 100 keys on every node, over and over for 2 s, which
    // is over 10 a second each on this machine (the rounds are counted).
    let warm_until = Instant::now() + Duration::from_millis(2000);
    let mut rounds = 0;
    while Instant::now() < warm_until || rounds < 40 {
        // A hundred requests in flight at a time, one node's round each.
        for n in 0..10 {
            let mut tasks = Vec::new();
            for p in &top {
                let addr = c.nodes[n].as_ref().unwrap().proxy_addr;
                let client = c.client.clone();
                let p = p.clone();
                tasks.push(tokio::spawn(async move {
                    http_on(&client, "GET", addr, &p, &[("host", "example.com")], "").await
                }));
            }
            for t in tasks {
                assert_eq!(t.await.unwrap().status, 200);
            }
        }
        rounds += 1;
    }
    let per_node: Vec<u64> = c
        .live()
        .map(|n| TierStats::get(&n.proxy.tier().unwrap().stats.hot_promoted))
        .collect();
    println!("{rounds} rounds; promoted per node {per_node:?}");
    assert!(
        c.stat(|s| &s.hot_promoted) >= 1_000,
        "{}",
        c.stat(|s| &s.hot_promoted)
    );
    // Now every node holds every top key: no peer hops at all.
    let hops = c.stat(|s| &s.owner_fetches);
    for n in 0..10 {
        for p in &top {
            assert_eq!(c.get(n, p).await, format!("body of {p}"));
        }
    }
    assert_eq!(
        c.stat(|s| &s.owner_fetches),
        hops,
        "peer hops for the top 100 keys"
    );
    assert_eq!(o.hits(), 100);
    // Once they cool, the copies on nodes that do not own them are dropped.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let mut owners = BTreeMap::new();
    let view = c.nodes[0].as_ref().unwrap().proxy.tier().unwrap().view();
    for p in &top {
        owners.insert(
            p.clone(),
            view.edges
                .owner(&format!("example.com{p}"))
                .unwrap()
                .id
                .clone(),
        );
    }
    // A request after the cooling counts each key again and ages the sketch,
    // which demotes the rest.
    for n in 0..10 {
        c.get(n, "/tick").await;
    }
    let mut held = 0;
    for (n, node) in c.nodes.iter().enumerate() {
        let node = node.as_ref().unwrap();
        for p in &top {
            let has = node
                .proxy
                .cache
                .memory()
                .get(&format!("example.com{p}"))
                .is_some();
            if owners[p] == c.ids[n] {
                assert!(has, "the owner keeps {p}");
            } else if has {
                held += 1;
            }
        }
    }
    assert_eq!(held, 0, "copies left on non-owners after cooling");
    assert!(c.stat(|s| &s.hot_demoted) >= 900);
    c.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_miss_for_a_dead_owner_goes_to_the_origin_until_swim_notices() {
    let o = origin().await;
    let mut c = Cluster::start(o.addr, 3, false, "").await;
    let view = c.nodes[0].as_ref().unwrap().proxy.tier().unwrap().view();
    let path = (0..100)
        .map(|i| format!("/owned/{i}"))
        .find(|p| view.edges.owner(&format!("example.com{p}")).unwrap().id == "edge-2")
        .unwrap();
    // Killed before anyone has a connection to it: a fetch from it is
    // refused at once, and node 0 answers from the origin instead.
    c.nodes[2].take().unwrap().abort();
    assert_eq!(c.get(0, &path).await, format!("body of {path}"));
    assert_eq!(c.stat(|s| &s.fallbacks), 1);
    assert_eq!(o.hits(), 1);
    // Once SWIM has removed it, node 0 owns the key or sends it to node 1.
    c.converge(2, Duration::from_secs(3)).await;
    c.get(1, &path).await;
    assert_eq!(c.stat(|s| &s.fallbacks), 1);
    c.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forged_hop_header_is_ignored_and_never_reaches_the_origin() {
    let o = origin().await;
    let c = Cluster::start(o.addr, 2, false, "").await;
    let view = c.nodes[0].as_ref().unwrap().proxy.tier().unwrap().view();
    let path = (0..100)
        .map(|i| format!("/f/{i}"))
        .find(|p| view.edges.owner(&format!("example.com{p}")).unwrap().id == "edge-1")
        .unwrap();
    let addr = c.nodes[0].as_ref().unwrap().proxy_addr;
    let forged = [
        ("host", "example.com"),
        ("edgefront-tier", "edge-1 owner 00ff"),
    ];
    let got = http_on(&c.client, "GET", addr, &path, &forged, "").await;
    assert_eq!(got.status, 200);
    // Still sent to the owner, as an unsigned request would be.
    assert_eq!(c.stat(|s| &s.owner_fetches), 1);
    let seen = o.seen();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].headers.get("edgefront-tier").is_none(),
        "{:?}",
        seen[0].headers
    );
    c.shutdown().await;
}
