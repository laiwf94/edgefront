//! End to end: two edge nodes, each the proxy with the cache layer on one
//! of its two routes, over real TCP in front of one origin, with a purge
//! on one node relayed to the other through the signed peer protocol.
//!
//! Each node is put together from the workspace's parts as a deployment
//! would: `edgefront_proxy::server::Proxy` with `CacheLayer::route_filters`
//! on the proxy listener, and an admin listener on
//! `edgefront_core::admin::Router` that serves `POST /purge` and the peer
//! endpoint of `cachet::cluster::Cluster`.

mod common;

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use cachet::cluster::{Cluster, GossipEvent, GossipSettings, PEER_PATH, PurgeHost};
use cachet::config::CacheConfig;
use cachet::layer::{CacheLayer, LayerSettings, SystemClock};
use cachet::purge::{self, MAX_PURGE_BODY, PurgeTarget};
use cachet::store::{Cache, CacheSettings};
use common::{TOKEN, TestOrigin, eventually, handler, http, reply};
use edgefront_core::admin::{BearerToken, Router, json, problem, read_body};
use edgefront_core::config::Config;
use edgefront_core::config::loader::parse_base_url;
use edgefront_core::listener::{ServerLimits, serve};
use edgefront_proxy::server::Proxy;
use http::{Method, StatusCode};
use tokio::net::TcpListener;
use tokio::sync::watch;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

/// The cache a node's cluster purges, counting what peers applied.
struct Host {
    cache: Arc<Cache>,
    applied: AtomicUsize,
}

impl PurgeHost for Host {
    fn purge<'a>(
        &'a self,
        targets: &'a [PurgeTarget],
    ) -> Pin<Box<dyn Future<Output = usize> + Send + 'a>> {
        Box::pin(self.cache.purge(targets))
    }

    fn record(&self, event: GossipEvent) {
        if let GossipEvent::Applied { .. } = event {
            self.applied.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// What the admin routes share.
struct Admin {
    cluster: Arc<Cluster>,
    token: BearerToken,
}

struct Edge {
    proxy: SocketAddr,
    host: Arc<Host>,
    _stop: watch::Sender<bool>,
}

/// Starts a node named `name` on the bound `admin` listener, with
/// `peer` (name, admin address) as its one peer.
async fn edge(
    name: &str,
    origin: SocketAddr,
    admin: TcpListener,
    peer: (&str, SocketAddr),
) -> Edge {
    let config = format!(
        "{{\"listen\":\"127.0.0.1:0\",\"routes\":[\
         {{\"host\":\"*\",\"path_prefix\":\"/cached/\",\"cluster\":\"origin\"}},\
         {{\"host\":\"*\",\"path_prefix\":\"/\",\"cluster\":\"origin\"}}],\
         \"clusters\":[{{\"name\":\"origin\",\"endpoints\":[{{\"address\":\"{origin}\"}}]}}]}}"
    );
    let cfg = Config::from_json(config.as_bytes()).expect("a valid proxy config");
    let cache_cfg: CacheConfig =
        toml::from_str("routes = [{ host = \"*\", path_prefix = \"/cached/\" }]").unwrap();
    let policies = cache_cfg
        .resolve(&cfg.routes)
        .expect("a valid cache config");
    let cache = Arc::new(Cache::new(&CacheSettings::default()).unwrap());
    let layer = CacheLayer::new(
        cache.clone(),
        policies,
        LayerSettings {
            name: name.into(),
            ..LayerSettings::default()
        },
        Arc::new(SystemClock),
    );
    let proxy = Arc::new(Proxy::new(&cfg).with_filters(layer.route_filters()));
    let host = Arc::new(Host {
        cache,
        applied: AtomicUsize::new(0),
    });
    let peers = [(
        peer.0.to_owned(),
        parse_base_url(&format!("http://{}", peer.1)).unwrap(),
    )];
    let gossip = GossipSettings {
        retry_max: Duration::from_millis(200),
        ..GossipSettings::default()
    };
    let (cluster, _tasks) = Cluster::start(
        name,
        KEY.to_vec(),
        &peers,
        host.clone(),
        cachet::origin::client(Duration::from_secs(1)),
        gossip,
    );
    let router = Router::new(Arc::new(Admin {
        cluster,
        token: BearerToken::new(TOKEN),
    }))
    .health()
    .route(
        Method::POST,
        "/purge",
        |admin: Arc<Admin>, req, _peer| async move {
            if let Err(refusal) = admin.token.check(req.headers()) {
                return refusal.into_response();
            }
            let body = match read_body(req.into_body(), MAX_PURGE_BODY).await {
                Ok(b) => b,
                Err(refusal) => return refusal.into_response(),
            };
            let targets = match purge::parse_request(&body) {
                Ok(t) => t,
                Err(e) => return problem(StatusCode::BAD_REQUEST, &e),
            };
            let (removed, id) = admin.cluster.purge_local(&targets).await;
            json(
                StatusCode::OK,
                &serde_json::json!({ "removed": removed, "id": id }),
            )
        },
    )
    .any(PEER_PATH, |admin: Arc<Admin>, req, peer| async move {
        admin.cluster.receive(req, peer).await
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let (stop, rx) = watch::channel(false);
    let stopped = |mut rx: watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|s| *s).await;
    };
    let limits = ServerLimits::default();
    tokio::spawn(serve(listener, proxy, limits, stopped(rx.clone())));
    tokio::spawn(serve(admin, Arc::new(router), limits, stopped(rx)));
    Edge {
        proxy: proxy_addr,
        host,
        _stop: stop,
    }
}

/// A miss on `name` that stored the response. The origin's `Date` has
/// whole seconds, so the lifetime left is 300 s or, across a second
/// boundary, 299.
fn stored_miss(got: &common::Got, name: &str) {
    let status = got.cache_status();
    let ttl = status
        .strip_prefix(&format!("{name}; fwd=miss; fwd-status=200; stored; ttl="))
        .unwrap_or_else(|| panic!("not a stored miss on {name}: {status}"));
    assert!(ttl == "300" || ttl == "299", "{status}");
}

#[tokio::test]
async fn two_routes_and_a_purge_across_two_nodes() {
    // The origin numbers its answers, so a fresh fetch is visible.
    let served = Arc::new(AtomicUsize::new(0));
    let s = served.clone();
    let origin = TestOrigin::start(handler(move |_, _| {
        let n = s.fetch_add(1, Ordering::SeqCst) + 1;
        reply(
            200,
            &[("cache-control", "max-age=300"), ("surrogate-key", "news")],
            format!("version {n}"),
        )
    }))
    .await;
    let (admin_a, admin_b) = (
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
    );
    let (addr_a, addr_b) = (admin_a.local_addr().unwrap(), admin_b.local_addr().unwrap());
    let a = edge("edge-a", origin.addr, admin_a, ("edge-b", addr_b)).await;
    let b = edge("edge-b", origin.addr, admin_b, ("edge-a", addr_a)).await;

    // The cached route: a miss on each node, then hits.
    for (node, name) in [(&a, "edge-a"), (&b, "edge-b")] {
        let first = http("GET", node.proxy, "/cached/story", &[], "").await;
        stored_miss(&first, name);
        let again = http("GET", node.proxy, "/cached/story", &[], "").await;
        assert!(
            again
                .cache_status()
                .starts_with(&format!("{name}; hit; ttl=")),
            "{}",
            again.cache_status()
        );
        assert_eq!(again.body, first.body);
        assert_ne!(again.header("x-request-id"), first.header("x-request-id"));
        // Stored for the cache only; clients never see the tag.
        assert_eq!(again.header("surrogate-key"), "");
    }
    assert_eq!(origin.hits(), 2);

    // The other route has no cache block: every request reaches the origin.
    for n in 0..2 {
        let plain = http("GET", a.proxy, "/plain", &[], "").await;
        assert_eq!(plain.status, 200);
        assert_eq!(plain.cache_status(), "", "request {n}");
        assert_eq!(plain.header("surrogate-key"), "news", "passed on as sent");
    }
    assert_eq!(origin.hits(), 4);

    // A purge on edge-a removes the tag there and reaches edge-b.
    let auth = format!("Bearer {TOKEN}");
    let refused = http("POST", addr_a, "/purge", &[], r#"{"tags": ["news"]}"#).await;
    assert_eq!(refused.status, 401);
    let purged = http(
        "POST",
        addr_a,
        "/purge",
        &[("authorization", &auth)],
        r#"{"tags": ["news"]}"#,
    )
    .await;
    assert_eq!(purged.status, 200);
    let result: serde_json::Value = serde_json::from_slice(&purged.body).unwrap();
    assert_eq!(result["removed"], 1);
    eventually("the purge on edge-b", || {
        b.host.applied.load(Ordering::SeqCst) == 1
    })
    .await;
    assert!(b.host.cache.memory().is_empty());
    for (node, name) in [(&a, "edge-a"), (&b, "edge-b")] {
        let after = http("GET", node.proxy, "/cached/story", &[], "").await;
        stored_miss(&after, name);
    }
    assert_eq!(origin.hits(), 6);
    // edge-a's own purge comes back from edge-b and is not applied twice.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(a.host.applied.load(Ordering::SeqCst), 0);
    assert!(!a.host.cache.memory().is_empty(), "the refetch is kept");
}
