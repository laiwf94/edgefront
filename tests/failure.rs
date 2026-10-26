//! How cachet behaves when things go wrong.
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{
    Binary, Scratch, TestOrigin, binary_config, black_hole, handler, harness, http, http_on,
    node_config, raw_sequence, raw_server, reply, secrets,
};

/// Kill the binary with SIGKILL mid-life, leave a half-written temporary
/// file behind, take the origin down, and restart: the object comes back
/// from the disk tier.
#[tokio::test]
async fn crash_and_restart_serves_from_disk() {
    let o = TestOrigin::start(handler(|_, _| {
        reply(200, &[("cache-control", "max-age=300")], "survives a crash")
    }))
    .await;
    let dir = Scratch::new("crash");
    let disk = dir.0.join("cache");
    let extra = format!(
        "[cache.disk]\npath = \"{}\"\nmax_bytes = 100000000\n",
        disk.display()
    );
    let config = binary_config("edge-a", o.addr, &extra);

    let bin = Binary::start(&dir.0, &config, &[]);
    assert_eq!(
        http("GET", bin.proxy_addr, "/doc", &[], "").await.body,
        "survives a crash"
    );
    common::eventually_async("disk write", || async {
        let m = http("GET", bin.admin_addr, "/metrics", &[], "").await;
        String::from_utf8_lossy(&m.body).contains("cachet_disk_writes_total 1")
    })
    .await;
    bin.signal("KILL");
    drop(bin);
    std::fs::write(disk.join("tmp").join("torn-write"), b"CACHET\0\x01 partial").unwrap();
    o.stop();

    let bin = Binary::start(&dir.0, &config, &[]);
    let got = http("GET", bin.proxy_addr, "/doc", &[], "").await;
    assert_eq!(got.status, 200);
    assert_eq!(got.body, "survives a crash");
    assert!(
        got.cache_status().starts_with("edge-a; hit"),
        "{}",
        got.cache_status()
    );
    assert!(!disk.join("tmp").join("torn-write").exists());
}

#[tokio::test]
async fn a_hung_primary_fails_over_within_the_response_timeout() {
    let (hung, _task) = black_hole().await;
    let backup = TestOrigin::start(handler(|_, _| reply(200, &[], "from the secondary"))).await;
    let h = harness(&[hung, backup.addr], |_, _| {});
    let start = Instant::now();
    let got = h.get("/a", &[]).await;
    assert_eq!(got.body, "from the secondary");
    // The harness's response timeout is two seconds.
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn malformed_origin_responses_become_502() {
    let cases: [&'static [u8]; 4] = [
        b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\ncache-control: max-age=60\r\n\r\nshort",
        b"HTTP/1.1 200 OK\r\ncontent-length: banana\r\n\r\n",
        b"HTTP/1.1 999999 What\r\n\r\n",
        b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03",
    ];
    for raw in cases {
        let (addr, _task) = raw_server(raw).await;
        let h = harness(&[addr], |_, _| {});
        let got = h.get("/a", &[]).await;
        assert_eq!(got.status, 502, "{:?}", String::from_utf8_lossy(raw));
        assert!(h.proxy.cache.memory().is_empty());
    }
}

#[tokio::test]
async fn too_many_origin_headers_become_502() {
    let mut raw = String::from("HTTP/1.1 200 OK\r\ncontent-length: 0\r\n");
    for i in 0..200 {
        raw.push_str(&format!("x-h{i}: v\r\n"));
    }
    raw.push_str("\r\n");
    let raw: &'static [u8] = Box::leak(raw.into_bytes().into_boxed_slice());
    let (addr, _task) = raw_server(raw).await;
    let h = harness(&[addr], |_, _| {});
    assert_eq!(h.get("/a", &[]).await.status, 502);
}

#[tokio::test]
async fn a_truncated_body_falls_back_to_a_stale_copy() {
    let good: &'static [u8] =
        b"HTTP/1.1 200 OK\r\ncache-control: max-age=10, stale-if-error=60\r\ncontent-length: 9\r\nconnection: close\r\n\r\ngood copy";
    let cut: &'static [u8] =
        b"HTTP/1.1 200 OK\r\ncache-control: max-age=10\r\ncontent-length: 100\r\n\r\ncut off";
    let (addr, _task) = raw_sequence(vec![good, cut]).await;
    let h = harness(&[addr], |_, _| {});
    assert_eq!(h.get("/a", &[]).await.body, "good copy");
    h.clock.advance(Duration::from_secs(15));
    let got = h.get("/a", &[]).await;
    assert_eq!(got.body, "good copy");
    assert!(
        got.cache_status()
            .contains("detail-origin=protocol; detail=stale-if-error"),
        "{}",
        got.cache_status()
    );
}

/// Many clients over real connections, a small memory budget that forces
/// constant eviction, and purges in between: every answer must belong to
/// the URL that was asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_clients_evictions_and_purges() {
    let o = TestOrigin::start(handler(|parts, _| {
        let path = parts.uri.path().to_string();
        reply(
            200,
            &[("cache-control", "max-age=300"), ("surrogate-key", "all")],
            format!("{path}:{}", "x".repeat(4000)),
        )
    }))
    .await;
    let extra = "[cache]\nmemory_bytes = 1048576\nmax_object_bytes = 65536\nshards = 4\n";
    let node = Arc::new(
        cachet::node::start(&node_config("edge-a", &[o.addr], extra), &secrets())
            .await
            .unwrap(),
    );
    let mut tasks = Vec::new();
    for t in 0..32 {
        let node = node.clone();
        tasks.push(tokio::spawn(async move {
            let client = cachet::origin::client(Duration::from_secs(2));
            for i in 0..150 {
                let path = format!("/obj/{}", (i * 7 + t * 13) % 400);
                let path: &'static str = Box::leak(path.into_boxed_str());
                let got = http_on(&client, "GET", node.proxy_addr, path, &[], "").await;
                assert_eq!(got.status, 200);
                assert!(
                    got.body.starts_with(format!("{path}:").as_bytes()),
                    "wrong body for {path}"
                );
                if t == 0 && i % 25 == 0 {
                    let auth = format!("Bearer {}", common::TOKEN);
                    http(
                        "POST",
                        node.admin_addr,
                        "/purge",
                        &[("authorization", &auth)],
                        r#"{"tags": ["all"]}"#,
                    )
                    .await;
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let mem = node.proxy.cache.memory();
    assert!(mem.bytes() <= mem.capacity());
    assert!(
        node.proxy
            .cache
            .stats
            .memory_evictions
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
    );
    Arc::into_inner(node).unwrap().shutdown().await;
}
