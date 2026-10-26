mod common;

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use cachet::cluster::{PurgeMessage, sign};
use cachet::config::Secrets;
use cachet::node::{self, Node};
use common::{Scratch, TOKEN, TestOrigin, eventually, handler, http, node_config, reply, secrets};

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

async fn free_port() -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap()
}

async fn tagged_origin() -> TestOrigin {
    TestOrigin::start(handler(|_, _| {
        reply(
            200,
            &[("cache-control", "max-age=300"), ("surrogate-key", "news")],
            "page",
        )
    }))
    .await
}

/// Starts a node whose admin listener is at `admin` and whose peers are
/// the given (name, admin address) pairs.
async fn start(
    name: &str,
    admin: SocketAddr,
    origin: SocketAddr,
    peers: &[(&str, SocketAddr)],
    extra: &str,
) -> Node {
    let mut text = String::from("[gossip]\nretry_max_ms = 200\n");
    for (peer, addr) in peers {
        text.push_str(&format!(
            "[[peers]]\nname = \"{peer}\"\nurl = \"http://{addr}\"\n"
        ));
    }
    text.push_str(extra);
    let mut config = node_config(name, &[origin], &text);
    config.admin_listen = admin;
    node::start(&config, &secrets()).await.unwrap()
}

async fn purge(node: &Node, body: &'static str) -> serde_json::Value {
    let auth = format!("Bearer {TOKEN}");
    let got = http(
        "POST",
        node.admin_addr,
        "/purge",
        &[("authorization", &auth)],
        body,
    )
    .await;
    assert_eq!(got.status, 200);
    serde_json::from_slice(&got.body).unwrap()
}

async fn cached(node: &Node) -> bool {
    http("GET", node.proxy_addr, "/news", &[], "")
        .await
        .cache_status()
        .contains("hit")
}

#[tokio::test]
async fn a_purge_reaches_every_node() {
    let o = tagged_origin().await;
    let (a, b, c) = (free_port().await, free_port().await, free_port().await);
    let na = start("edge-a", a, o.addr, &[("edge-b", b), ("edge-c", c)], "").await;
    let nb = start("edge-b", b, o.addr, &[("edge-a", a), ("edge-c", c)], "").await;
    let nc = start("edge-c", c, o.addr, &[("edge-a", a), ("edge-b", b)], "").await;
    for n in [&na, &nb, &nc] {
        http("GET", n.proxy_addr, "/news", &[], "").await;
        assert!(cached(n).await);
    }
    let result = purge(&na, r#"{"tags": ["news"]}"#).await;
    assert_eq!(result["removed"], 1);
    assert_eq!(result["id"].as_str().unwrap().len(), 32);
    for n in [&nb, &nc] {
        eventually("purge on peer", || {
            n.proxy.metrics.purges_peer.load(Ordering::Relaxed) == 1
        })
        .await;
        assert!(!cached(n).await);
    }
    // Relayed copies come back to edge-a and are recognised, not reapplied.
    assert_eq!(na.proxy.metrics.purges_peer.load(Ordering::Relaxed), 0);
    eventually("deliveries counted", || {
        na.proxy.metrics.gossip_sent.load(Ordering::Relaxed) == 2
    })
    .await;
    for n in [na, nb, nc] {
        n.shutdown().await;
    }
}

#[tokio::test]
async fn a_purge_spreads_through_a_relay() {
    let o = tagged_origin().await;
    let (a, b, c) = (free_port().await, free_port().await, free_port().await);
    // edge-a cannot reach edge-c directly; edge-b knows both.
    let na = start("edge-a", a, o.addr, &[("edge-b", b)], "").await;
    let nb = start("edge-b", b, o.addr, &[("edge-a", a), ("edge-c", c)], "").await;
    let nc = start("edge-c", c, o.addr, &[("edge-b", b)], "").await;
    http("GET", nc.proxy_addr, "/news", &[], "").await;
    purge(&na, r#"{"urls": ["http://example.com/news"]}"#).await;
    eventually("purge relayed to edge-c", || {
        nc.proxy.metrics.purges_peer.load(Ordering::Relaxed) == 1
    })
    .await;
    assert!(!cached(&nc).await);
    for n in [na, nb, nc] {
        n.shutdown().await;
    }
}

#[tokio::test]
async fn a_peer_that_was_down_gets_the_purge_when_it_returns() {
    let o = tagged_origin().await;
    let (a, b) = (free_port().await, free_port().await);
    let dir = Scratch::new("partition");
    let disk = format!(
        "[cache.disk]\npath = \"{}\"\nmax_bytes = 100000000\n",
        dir.0.display()
    );
    let nb = start("edge-b", b, o.addr, &[("edge-a", a)], &disk).await;
    let b_proxy = nb.proxy_addr;
    http("GET", b_proxy, "/news", &[], "").await;
    nb.shutdown().await;

    let na = start("edge-a", a, o.addr, &[("edge-b", b)], "").await;
    purge(&na, r#"{"tags": ["news"]}"#).await;
    eventually("delivery retried", || {
        na.proxy.metrics.gossip_retries.load(Ordering::Relaxed) >= 2
    })
    .await;

    // edge-b comes back with its disk tier still holding the purged page.
    let nb = start("edge-b", b, o.addr, &[("edge-a", a)], &disk).await;
    eventually("purge delivered after the partition", || {
        nb.proxy.metrics.purges_peer.load(Ordering::Relaxed) == 1
    })
    .await;
    assert!(!cached(&nb).await);
    assert_eq!(na.proxy.metrics.gossip_sent.load(Ordering::Relaxed), 1);
    na.shutdown().await;
    nb.shutdown().await;
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn message(id: &str, issued_at: u64) -> String {
    let msg = PurgeMessage {
        id: id.into(),
        origin: "edge-x".into(),
        issued_at,
        tags: vec!["news".into()],
        keys: vec![],
    };
    serde_json::to_string(&msg).unwrap()
}

async fn deliver(node: &Node, body: String, key: &[u8]) -> (u16, String) {
    let sig = sign(key, body.as_bytes());
    let body: &'static str = Box::leak(body.into_boxed_str());
    let got = http(
        "POST",
        node.admin_addr,
        "/peer/purge",
        &[("x-cachet-signature", &sig), ("x-cachet-node", "edge-x")],
        body,
    )
    .await;
    (
        got.status.as_u16(),
        String::from_utf8_lossy(&got.body).into_owned(),
    )
}

#[tokio::test]
async fn peer_messages_must_be_signed_fresh_and_new() {
    let o = tagged_origin().await;
    let n = start("edge-a", free_port().await, o.addr, &[], "").await;
    http("GET", n.proxy_addr, "/news", &[], "").await;

    let body = message("m1", now());
    assert_eq!(
        deliver(&n, body.clone(), b"wrong key wrong key wrong key wr")
            .await
            .0,
        401
    );
    let unsigned = http("POST", n.admin_addr, "/peer/purge", &[], "{}").await;
    assert_eq!(unsigned.status, 401);
    assert!(cached(&n).await);

    assert_eq!(
        deliver(&n, body.clone(), KEY).await,
        (200, r#"{"removed":1}"#.into())
    );
    assert_eq!(
        deliver(&n, body, KEY).await,
        (200, r#"{"duplicate":true}"#.into())
    );
    assert_eq!(n.proxy.metrics.purges_peer.load(Ordering::Relaxed), 1);

    assert!(
        deliver(&n, message("m2", now() - 700), KEY)
            .await
            .1
            .contains("expired")
    );
    assert!(
        deliver(&n, message("m3", now() + 600), KEY)
            .await
            .1
            .contains("future")
    );
    assert!(
        deliver(&n, "{\"id\": 1}".into(), KEY)
            .await
            .1
            .contains("malformed")
    );
    let bad_tag = message("m4", now()).replace("\"news\"", "\"has space\"");
    assert!(deliver(&n, bad_tag, KEY).await.1.contains("invalid tag"));
    let own = message("m5", now()).replace("edge-x", "edge-a");
    assert_eq!(deliver(&n, own, KEY).await.1, r#"{"duplicate":true}"#);
    let huge = format!("{{\"pad\": \"{}\"}}", "x".repeat(200_000));
    assert_eq!(deliver(&n, huge, KEY).await.0, 413);
    assert_eq!(
        http("GET", n.admin_addr, "/peer/purge", &[], "")
            .await
            .status,
        405
    );
    assert_eq!(n.proxy.metrics.gossip_rejected.load(Ordering::Relaxed), 6);
    n.shutdown().await;
}

#[tokio::test]
async fn nodes_without_a_cluster_key_refuse_peer_messages() {
    let o = tagged_origin().await;
    let n = node::start(&node_config("edge-a", &[o.addr], ""), &Secrets::default())
        .await
        .unwrap();
    assert_eq!(
        http("POST", n.admin_addr, "/peer/purge", &[], "{}")
            .await
            .status,
        404
    );
    n.shutdown().await;
}

#[tokio::test]
async fn a_peer_with_another_key_is_not_retried() {
    let o = tagged_origin().await;
    let (a, b) = (free_port().await, free_port().await);
    let na = start("edge-a", a, o.addr, &[("edge-b", b)], "").await;
    let mut config = node_config(
        "edge-b",
        &[o.addr],
        "[[peers]]\nname = \"edge-a\"\nurl = \"http://127.0.0.1:1\"\n",
    );
    config.admin_listen = b;
    let other = Secrets {
        admin_token: None,
        cluster_secret: Some(b"another key another key another k".to_vec()),
    };
    let nb = node::start(&config, &other).await.unwrap();
    purge(&na, r#"{"tags": ["news"]}"#).await;
    eventually("refused delivery dropped", || {
        na.proxy.metrics.gossip_dropped.load(Ordering::Relaxed) == 1
    })
    .await;
    assert_eq!(na.proxy.metrics.gossip_retries.load(Ordering::Relaxed), 0);
    assert_eq!(nb.proxy.metrics.gossip_rejected.load(Ordering::Relaxed), 1);
    na.shutdown().await;
    nb.shutdown().await;
}
