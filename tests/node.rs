mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use cachet::config::Secrets;
use cachet::node;
use common::{TOKEN, TestOrigin, handler, http, node_config, reply, secrets};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn bearer() -> String {
    format!("Bearer {TOKEN}")
}

#[tokio::test]
async fn serves_over_tcp_and_purges_through_the_admin_api() {
    let o = TestOrigin::start(handler(|_, _| {
        reply(
            200,
            &[
                ("cache-control", "max-age=60"),
                ("surrogate-key", "product-7"),
            ],
            "page",
        )
    }))
    .await;
    let n = node::start(&node_config("edge-a", &[o.addr], ""), &secrets())
        .await
        .unwrap();
    let first = http("GET", n.proxy_addr, "/p/7", &[], "").await;
    assert_eq!(first.body, "page");
    assert!(first.cache_status().starts_with("edge-a; fwd=miss"));
    assert!(
        http("GET", n.proxy_addr, "/p/7", &[], "")
            .await
            .cache_status()
            .starts_with("edge-a; hit")
    );

    let auth = bearer();
    let purged = http(
        "POST",
        n.admin_addr,
        "/purge",
        &[("authorization", &auth)],
        r#"{"tags": ["product-7"]}"#,
    )
    .await;
    assert_eq!(purged.status, 200);
    let purged: serde_json::Value = serde_json::from_slice(&purged.body).unwrap();
    assert_eq!(
        (purged["removed"].as_u64(), purged["targets"].as_u64()),
        (Some(1), Some(1))
    );
    assert!(
        http("GET", n.proxy_addr, "/p/7", &[], "")
            .await
            .cache_status()
            .contains("fwd=miss")
    );
    let by_url = http(
        "POST",
        n.admin_addr,
        "/purge",
        &[("authorization", &auth)],
        r#"{"urls": ["http://example.com/p/7"]}"#,
    )
    .await;
    let by_url: serde_json::Value = serde_json::from_slice(&by_url.body).unwrap();
    assert_eq!(by_url["removed"], 1);
    assert_eq!(o.hits(), 2);
    assert_eq!(n.proxy.metrics.purges_local.load(Ordering::Relaxed), 2);
    n.shutdown().await;
}

#[tokio::test]
async fn admin_endpoints_and_their_errors() {
    let o = TestOrigin::start(handler(|_, _| reply(200, &[], "x"))).await;
    let n = node::start(&node_config("edge-a", &[o.addr], ""), &secrets())
        .await
        .unwrap();
    let a = n.admin_addr;
    assert_eq!(http("GET", a, "/healthz", &[], "").await.body, "ok\n");
    http("GET", n.proxy_addr, "/warm", &[], "").await;
    let metrics = http("GET", a, "/metrics", &[], "").await;
    assert_eq!(metrics.header("content-type"), "text/plain; version=0.0.4");
    let text = String::from_utf8(metrics.body.to_vec()).unwrap();
    assert!(
        text.contains("cachet_requests_total{outcome=\"miss\"} 1"),
        "{text}"
    );
    assert!(text.contains("cachet_origin_up{origin=\"origin0\"} 1"));

    let body = r#"{"tags": ["a"]}"#;
    let missing = http("POST", a, "/purge", &[], body).await;
    assert_eq!(missing.status, 401);
    assert_eq!(missing.header("www-authenticate"), "Bearer");
    assert_eq!(
        http(
            "POST",
            a,
            "/purge",
            &[("authorization", "Bearer wrong")],
            body
        )
        .await
        .status,
        401
    );
    assert_eq!(
        http("POST", a, "/purge", &[("authorization", TOKEN)], body)
            .await
            .status,
        401
    );
    let auth = bearer();
    let bad = http("POST", a, "/purge", &[("authorization", &auth)], "{").await;
    assert_eq!(bad.status, 400);
    assert!(String::from_utf8_lossy(&bad.body).contains("invalid purge request"));
    let huge: &'static str =
        Box::leak(format!(r#"{{"tags": ["{}"]}}"#, "x".repeat(70_000)).into_boxed_str());
    assert_eq!(
        http("POST", a, "/purge", &[("authorization", &auth)], huge)
            .await
            .status,
        413
    );
    assert_eq!(http("GET", a, "/purge", &[], "").await.status, 405);
    assert_eq!(http("GET", a, "/other", &[], "").await.status, 404);
    n.shutdown().await;
}

#[tokio::test]
async fn purge_is_off_without_a_token() {
    let o = TestOrigin::start(handler(|_, _| reply(200, &[], "x"))).await;
    let n = node::start(&node_config("edge-a", &[o.addr], ""), &Secrets::default())
        .await
        .unwrap();
    let got = http(
        "POST",
        n.admin_addr,
        "/purge",
        &[("authorization", "Bearer anything")],
        r#"{"tags":["a"]}"#,
    )
    .await;
    assert_eq!(got.status, 403);
    n.shutdown().await;
}

#[tokio::test]
async fn oversized_request_heads_are_refused() {
    let o = TestOrigin::start(handler(|_, _| reply(200, &[], "x"))).await;
    let n = node::start(
        &node_config("edge-a", &[o.addr], "[limits]\nmax_header_bytes = 8192\n"),
        &secrets(),
    )
    .await
    .unwrap();
    let mut s = TcpStream::connect(n.proxy_addr).await.unwrap();
    let head = format!(
        "GET / HTTP/1.1\r\nhost: example.com\r\nx-big: {}\r\n\r\n",
        "a".repeat(20_000)
    );
    let _ = s.write_all(head.as_bytes()).await;
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), s.read_to_end(&mut buf)).await;
    let text = String::from_utf8_lossy(&buf);
    assert!(text.starts_with("HTTP/1.1 431"), "{text}");
    assert_eq!(o.hits(), 0);
    n.shutdown().await;
}

#[tokio::test]
async fn garbage_from_a_client_gets_400() {
    let o = TestOrigin::start(handler(|_, _| reply(200, &[], "x"))).await;
    let n = node::start(&node_config("edge-a", &[o.addr], ""), &secrets())
        .await
        .unwrap();
    let mut s = TcpStream::connect(n.proxy_addr).await.unwrap();
    s.write_all(b"\x00\x01\x02 nonsense\r\n\r\n").await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), s.read_to_end(&mut buf)).await;
    assert!(String::from_utf8_lossy(&buf).starts_with("HTTP/1.1 400"));
    n.shutdown().await;
}

#[tokio::test]
async fn shutdown_lets_open_requests_finish() {
    let o = TestOrigin::start(handler(|_, _| reply(200, &[], "slow"))).await;
    o.set_delay(Duration::from_millis(300));
    let n = node::start(&node_config("edge-a", &[o.addr], ""), &secrets())
        .await
        .unwrap();
    let addr = n.proxy_addr;
    let in_flight = tokio::spawn(async move { http("GET", addr, "/slow", &[], "").await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    n.shutdown().await;
    let got = in_flight.await.unwrap();
    assert_eq!(got.status, 200);
    assert_eq!(got.body, "slow");
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "still accepting after shutdown"
    );
}

/// Reads one response from a keep-alive connection: the head up to the
/// blank line, then a `content-length` body.
async fn read_response(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        assert_eq!(s.read(&mut byte).await.unwrap(), 1, "closed mid-head");
        buf.push(byte[0]);
    }
    let head = String::from_utf8(buf).unwrap();
    let len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await.unwrap();
    head + &String::from_utf8(body).unwrap()
}

/// How long a connection that has just had a response takes to be closed
/// by the node, if it is closed within `limit`.
async fn closed_within(s: &mut TcpStream, limit: Duration) -> Option<Duration> {
    let started = std::time::Instant::now();
    let mut rest = Vec::new();
    match tokio::time::timeout(limit, s.read_to_end(&mut rest)).await {
        Ok(_) => {
            assert!(rest.is_empty(), "unexpected bytes: {rest:?}");
            Some(started.elapsed())
        }
        Err(_) => None,
    }
}

const GET_KEEP_ALIVE: &[u8] = b"GET /k HTTP/1.1\r\nhost: example.com\r\n\r\n";

#[tokio::test]
async fn silent_keep_alive_connections_close_after_the_header_timeout() {
    let o = TestOrigin::start(handler(|_, _| reply(200, &[], "k"))).await;
    let mut c = node_config("edge-a", &[o.addr], "");
    c.timeouts.client_header_ms = 300;
    let n = node::start(&c, &secrets()).await.unwrap();

    // As in cachet's accept loop: with no idle_ms, a keep-alive connection
    // that stays silent for client_header_ms is closed.
    let mut s = TcpStream::connect(n.proxy_addr).await.unwrap();
    s.write_all(GET_KEEP_ALIVE).await.unwrap();
    assert!(read_response(&mut s).await.ends_with("\r\n\r\nk"));
    let waited = closed_within(&mut s, Duration::from_secs(5)).await.unwrap();
    assert!(waited >= Duration::from_millis(200), "{waited:?}");

    // Half a head, then silence: closed without a response.
    let mut s = TcpStream::connect(n.proxy_addr).await.unwrap();
    s.write_all(b"GET /k HTTP/1.1\r\nhost").await.unwrap();
    let waited = closed_within(&mut s, Duration::from_secs(5)).await.unwrap();
    assert!(waited >= Duration::from_millis(200), "{waited:?}");
    n.shutdown().await;
}

#[tokio::test]
async fn idle_ms_keeps_connections_open_longer_than_the_header_timeout() {
    let o = TestOrigin::start(handler(|_, _| reply(200, &[], "k"))).await;
    let mut c = node_config("edge-a", &[o.addr], "");
    c.timeouts.client_header_ms = 200;
    c.timeouts.idle_ms = Some(5_000);
    let n = node::start(&c, &secrets()).await.unwrap();
    let mut s = TcpStream::connect(n.proxy_addr).await.unwrap();
    s.write_all(GET_KEEP_ALIVE).await.unwrap();
    read_response(&mut s).await;
    // Well past the header timeout, the idle connection is still served.
    tokio::time::sleep(Duration::from_millis(600)).await;
    s.write_all(GET_KEEP_ALIVE).await.unwrap();
    assert!(read_response(&mut s).await.ends_with("\r\n\r\nk"));
    // A head that starts and stalls still gets only the header timeout.
    s.write_all(b"GET /k HTTP/1.1\r\n").await.unwrap();
    let waited = closed_within(&mut s, Duration::from_secs(3)).await.unwrap();
    assert!(waited < Duration::from_secs(2), "{waited:?}");
    n.shutdown().await;
}

#[tokio::test]
async fn shutdown_closes_idle_connections_and_ends_busy_ones_after_their_request() {
    let o = TestOrigin::start(handler(|_, _| reply(200, &[], "slow"))).await;
    o.set_delay(Duration::from_millis(400));
    let n = node::start(&node_config("edge-a", &[o.addr], ""), &secrets())
        .await
        .unwrap();
    let mut idle = TcpStream::connect(n.proxy_addr).await.unwrap();
    let mut busy = TcpStream::connect(n.proxy_addr).await.unwrap();
    busy.write_all(GET_KEEP_ALIVE).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = std::time::Instant::now();
    let shutdown = tokio::spawn(n.shutdown());

    // The idle connection is closed at once, the busy one gets its response
    // with `Connection: close` and is then closed.
    let waited = closed_within(&mut idle, Duration::from_secs(1))
        .await
        .unwrap();
    assert!(waited < Duration::from_millis(300), "{waited:?}");
    let resp = read_response(&mut busy).await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(
        resp.to_ascii_lowercase().contains("connection: close"),
        "{resp}"
    );
    assert!(resp.ends_with("\r\n\r\nslow"), "{resp}");
    assert!(
        closed_within(&mut busy, Duration::from_secs(1))
            .await
            .is_some()
    );
    shutdown.await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn shutdown_closes_requests_still_open_after_shutdown_ms() {
    let o = TestOrigin::start(handler(|_, _| reply(200, &[], "late"))).await;
    o.set_delay(Duration::from_secs(10));
    let mut c = node_config("edge-a", &[o.addr], "");
    c.timeouts.shutdown_ms = 300;
    c.timeouts.response_ms = 30_000;
    let n = node::start(&c, &secrets()).await.unwrap();
    let mut s = TcpStream::connect(n.proxy_addr).await.unwrap();
    s.write_all(GET_KEEP_ALIVE).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = std::time::Instant::now();
    n.shutdown().await;
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(250), "{waited:?}");
    assert!(waited < Duration::from_secs(3), "{waited:?}");
    // The hung request's connection was closed, not left running.
    assert!(
        closed_within(&mut s, Duration::from_secs(1))
            .await
            .is_some()
    );
}

#[tokio::test]
async fn startup_failures_are_reported() {
    let o = TestOrigin::start(handler(|_, _| reply(200, &[], "x"))).await;
    let peers = "[[peers]]\nname = \"edge-b\"\nurl = \"http://127.0.0.1:1\"\n";
    let err = node::start(
        &node_config("edge-a", &[o.addr], peers),
        &Secrets::default(),
    )
    .await
    .err()
    .unwrap();
    assert!(err.contains("CACHET_CLUSTER_SECRET"));

    let blocker = std::env::temp_dir().join(format!("cachet-blocked-{}", std::process::id()));
    std::fs::write(&blocker, b"file").unwrap();
    let disk = format!(
        "[cache.disk]\npath = \"{}\"\nmax_bytes = 100000000\n",
        blocker.display()
    );
    let err = node::start(&node_config("edge-a", &[o.addr], &disk), &secrets())
        .await
        .err()
        .unwrap();
    assert!(err.contains("disk tier"), "{err}");
    std::fs::remove_file(&blocker).unwrap();

    let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = node_config("edge-a", &[o.addr], "");
    config.listen = taken.local_addr().unwrap();
    let err = node::start(&config, &secrets()).await.err().unwrap();
    assert!(err.contains("cannot listen"), "{err}");
}

#[tokio::test]
async fn starts_on_listeners_it_is_given() {
    let o = TestOrigin::start(handler(|_, _| {
        reply(200, &[("cache-control", "max-age=60")], "page")
    }))
    .await;
    // The config names ports that are taken; the node never binds them.
    let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = node_config("edge-a", &[o.addr], "");
    config.listen = taken.local_addr().unwrap();
    config.admin_listen = taken.local_addr().unwrap();
    let listeners = node::Listeners {
        proxy: tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
        admin: tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
    };
    let (proxy, admin) = (
        listeners.proxy.local_addr().unwrap(),
        listeners.admin.local_addr().unwrap(),
    );
    let n = node::start_with(&config, &secrets(), listeners)
        .await
        .unwrap();
    assert_eq!((n.proxy_addr, n.admin_addr), (proxy, admin));
    http("GET", proxy, "/a", &[], "").await;
    assert!(
        http("GET", proxy, "/a", &[], "")
            .await
            .cache_status()
            .contains("hit")
    );
    assert_eq!(http("GET", admin, "/healthz", &[], "").await.status, 200);
    n.shutdown().await;

    // A config that fails validation is refused before the listeners serve.
    let listeners = node::Listeners {
        proxy: tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
        admin: tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
    };
    let peers = "[[peers]]\nname = \"edge-b\"\nurl = \"http://127.0.0.1:1\"\n";
    let err = node::start_with(
        &node_config("edge-a", &[o.addr], peers),
        &Secrets::default(),
        listeners,
    )
    .await
    .err()
    .unwrap();
    assert!(err.contains("CACHET_CLUSTER_SECRET"), "{err}");
}
