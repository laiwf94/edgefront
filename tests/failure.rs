//! How cachet behaves when things go wrong.
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{
    Binary, Scratch, TestOrigin, binary_config, black_hole, handler, harness, http, http_on,
    node_config, raw_sequence, raw_server, reply, secrets,
};

/// Kill the binary with SIGKILL mid-life, leave a half-written record at
/// the end of the segment it was writing, take the origin down, and
/// restart: the object comes back from the disk tier and the torn record is
/// cut off.
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
    let segment = std::fs::read_dir(disk.join("segments"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .max()
        .unwrap();
    let whole = std::fs::metadata(&segment).unwrap().len();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&segment)
        .unwrap();
    std::io::Write::write_all(&mut file, b"EFS1\x01\x00\x00\x10\x00 partial").unwrap();
    drop(file);
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
    assert_eq!(std::fs::metadata(&segment).unwrap().len(), whole);
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

/// An origin that answers the first request on each connection, keeps it
/// open, then closes it on the second without an answer: what a pooled
/// connection looks like when the origin's keep-alive timeout ends it just
/// as the proxy reuses it. Returns the address and the requests seen.
async fn closes_reused_connections() -> (std::net::SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // The first request: its head, then its declared body.
                let head_end = loop {
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .map_or(0, |v| v.trim().parse().unwrap());
                while buf.len() < head_end + len {
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let body = String::from_utf8_lossy(&buf[head_end..head_end + len]).into_owned();
                log.lock()
                    .unwrap()
                    .push(format!("{} {body}", head.lines().next().unwrap()));
                let _ = s
                    .write_all(b"HTTP/1.1 201 Created\r\ncontent-length: 2\r\n\r\nok")
                    .await;
                // The second request on this connection: closed unanswered.
                let _ = s.read(&mut chunk).await;
            });
        }
    });
    (addr, seen)
}

#[tokio::test]
async fn a_small_idempotent_body_is_sent_again_when_a_reused_connection_was_closed() {
    let (addr, seen) = closes_reused_connections().await;
    let h = harness(&[addr], |_, _| {});
    let put = |body: &'static str| {
        let len = body.len().to_string();
        let h = &h;
        async move {
            h.send(
                "PUT",
                "/config/a",
                &[("content-length", len.as_str())],
                body,
            )
            .await
        }
    };
    assert_eq!(put("first").await.status, 201);
    // The pooled connection is reused and closed under the request; the
    // body is sent again on a fresh connection.
    let got = put("second").await;
    assert_eq!(got.status, 201);
    assert_eq!(got.body, "ok");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        [
            "put /config/a http/1.1 first",
            "put /config/a http/1.1 second"
        ]
    );
}

#[tokio::test]
async fn a_post_body_is_sent_once() {
    let (addr, seen) = closes_reused_connections().await;
    let h = harness(&[addr], |_, _| {});
    let post = || h.send("POST", "/form", &[("content-length", "4")], "data");
    assert_eq!(post().await.status, 201);
    // POST is not idempotent, so the request that met a closed connection
    // is not sent again.
    assert_eq!(post().await.status, 502);
    assert_eq!(seen.lock().unwrap().len(), 1);
}
