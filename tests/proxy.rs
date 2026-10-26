mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{TestOrigin, black_hole, closed_port, handler, harness, reply, stalled_body};

async fn origin_with(
    headers: &'static [(&'static str, &'static str)],
    body: &'static str,
) -> TestOrigin {
    TestOrigin::start(handler(move |_, _| reply(200, headers, body))).await
}

#[tokio::test]
async fn miss_then_hit() {
    let o = origin_with(
        &[
            ("cache-control", "max-age=60"),
            ("content-type", "text/plain"),
        ],
        "hello",
    )
    .await;
    let h = harness(&[o.addr], |_, _| {});
    let first = h.get("/a", &[]).await;
    assert_eq!(first.status, 200);
    assert_eq!(first.body, "hello");
    assert_eq!(
        first.cache_status(),
        "test-node; fwd=miss; fwd-status=200; stored; ttl=60"
    );
    assert_eq!(first.header("x-request-id").len(), 32);
    let second = h.get("/a", &[]).await;
    assert_eq!(second.body, "hello");
    assert!(
        second.cache_status().starts_with("test-node; hit; ttl="),
        "{}",
        second.cache_status()
    );
    assert_eq!(second.header("content-type"), "text/plain");
    assert_eq!(second.header("accept-ranges"), "bytes");
    assert_eq!(o.hits(), 1);
    // Different hosts and query strings are different objects.
    h.get("/a", &[("host", "other.example")]).await;
    h.get("/a?x=1", &[]).await;
    assert_eq!(o.hits(), 3);
}

#[tokio::test]
async fn origin_sees_forwarding_headers_but_not_client_conditionals() {
    let o = origin_with(
        &[("cache-control", "max-age=60"), ("etag", "\"v1\"")],
        "0123456789",
    )
    .await;
    let h = harness(&[o.addr], |_, _| {});
    let got = h
        .get(
            "/a",
            &[
                ("if-none-match", "\"v1\""),
                ("range", "bytes=0-1"),
                ("connection", "x-secret"),
                ("x-secret", "1"),
                ("x-request-id", "trace-1"),
            ],
        )
        .await;
    // The cache answered the client's conditional itself, from the stored copy.
    assert_eq!(got.status, 304);
    let seen = o.last();
    for absent in ["if-none-match", "range", "x-secret", "connection"] {
        assert!(seen.headers.get(absent).is_none(), "{absent} was forwarded");
    }
    assert_eq!(seen.headers["via"], "1.1 test-node");
    assert_eq!(seen.headers["x-forwarded-for"], "192.0.2.1");
    assert_eq!(seen.headers["x-request-id"], "trace-1");
    assert_eq!(seen.headers["host"], "example.com");
    let ranged = h.get("/a", &[("range", "bytes=2-3")]).await;
    assert_eq!(ranged.status, 206);
    assert_eq!(ranged.body, "23");
    assert_eq!(o.hits(), 1);
}

#[tokio::test]
async fn hop_by_hop_and_surrogate_headers_do_not_reach_clients() {
    let o = origin_with(
        &[
            ("cache-control", "max-age=60"),
            ("connection", "x-hop"),
            ("x-hop", "1"),
            ("surrogate-key", "a b"),
        ],
        "x",
    )
    .await;
    let h = harness(&[o.addr], |_, _| {});
    for _ in 0..2 {
        let got = h.get("/a", &[]).await;
        assert!(got.headers.get("x-hop").is_none());
        assert!(got.headers.get("surrogate-key").is_none());
    }
}

#[tokio::test]
async fn responses_that_must_not_be_shared_are_not_stored() {
    for headers in [
        &[("cache-control", "private, max-age=60")][..],
        &[("cache-control", "no-store")][..],
        &[("cache-control", "max-age=60"), ("set-cookie", "session=1")][..],
        &[("cache-control", "max-age=60"), ("vary", "*")][..],
        &[][..],
    ] {
        let o = TestOrigin::start(handler(move |_, _| reply(200, headers, "personal"))).await;
        let h = harness(&[o.addr], |_, _| {});
        for _ in 0..2 {
            let got = h.get("/me", &[]).await;
            assert_eq!(got.body, "personal");
            assert_eq!(got.cache_status(), "test-node; fwd=miss; fwd-status=200");
        }
        assert_eq!(o.hits(), 2, "{headers:?}");
    }
}

#[tokio::test]
async fn authorization_needs_permission_from_the_origin() {
    let o = origin_with(&[("cache-control", "max-age=60")], "x").await;
    let h = harness(&[o.addr], |_, _| {});
    h.get("/a", &[("authorization", "Bearer t")]).await;
    h.get("/a", &[("authorization", "Bearer t")]).await;
    assert_eq!(o.hits(), 2);
    let o = origin_with(&[("cache-control", "public, max-age=60")], "x").await;
    let h = harness(&[o.addr], |_, _| {});
    h.get("/a", &[("authorization", "Bearer t")]).await;
    h.get("/a", &[("authorization", "Bearer t")]).await;
    assert_eq!(o.hits(), 1);
}

#[tokio::test]
async fn vary_keeps_one_variant_per_selecting_value() {
    let o = TestOrigin::start(handler(|parts, _| {
        let lang = parts
            .headers
            .get("accept-language")
            .map_or("none", |v| v.to_str().unwrap())
            .to_string();
        reply(
            200,
            &[("cache-control", "max-age=60"), ("vary", "accept-language")],
            lang,
        )
    }))
    .await;
    let h = harness(&[o.addr], |_, _| {});
    for _ in 0..3 {
        assert_eq!(h.get("/a", &[("accept-language", "en")]).await.body, "en");
        assert_eq!(h.get("/a", &[("accept-language", "fr")]).await.body, "fr");
        assert_eq!(h.get("/a", &[]).await.body, "none");
    }
    assert_eq!(o.hits(), 3);
}

#[tokio::test]
async fn concurrent_misses_make_one_origin_request() {
    let o = origin_with(&[("cache-control", "max-age=60")], "shared").await;
    o.set_delay(Duration::from_millis(300));
    let h = Arc::new(harness(&[o.addr], |_, _| {}));
    let mut tasks = Vec::new();
    for _ in 0..50 {
        let h = h.clone();
        tasks.push(tokio::spawn(async move { h.get("/hot", &[]).await }));
    }
    let mut collapsed = 0;
    for t in tasks {
        let got = t.await.unwrap();
        assert_eq!(got.body, "shared");
        collapsed += usize::from(got.cache_status().contains("collapsed"));
    }
    assert_eq!(o.hits(), 1);
    assert_eq!(collapsed, 49);
    assert_eq!(
        h.proxy
            .metrics
            .collapsed
            .load(std::sync::atomic::Ordering::Relaxed),
        49
    );
}

#[tokio::test]
async fn collapsed_requests_never_share_a_private_response() {
    let o = TestOrigin::start(handler(|parts, _| {
        let who = parts
            .headers
            .get("cookie")
            .map_or("nobody", |v| v.to_str().unwrap())
            .to_string();
        reply(200, &[("cache-control", "private")], who)
    }))
    .await;
    o.set_delay(Duration::from_millis(200));
    let h = Arc::new(harness(&[o.addr], |_, _| {}));
    let mut tasks = Vec::new();
    for i in 0..10 {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            let cookie = format!("user={i}");
            let got = h.get("/account", &[("cookie", &cookie)]).await;
            (cookie, got)
        }));
    }
    for t in tasks {
        let (cookie, got) = t.await.unwrap();
        assert_eq!(
            got.body,
            cookie.as_bytes(),
            "a response went to the wrong client"
        );
    }
    assert_eq!(o.hits(), 10);
}

#[tokio::test]
async fn collapsed_follower_with_another_variant_fetches_its_own() {
    let o = TestOrigin::start(handler(|parts, _| {
        let enc = parts
            .headers
            .get("accept-encoding")
            .map_or("identity", |v| v.to_str().unwrap())
            .to_string();
        reply(
            200,
            &[("cache-control", "max-age=60"), ("vary", "accept-encoding")],
            enc,
        )
    }))
    .await;
    o.set_delay(Duration::from_millis(200));
    let h = Arc::new(harness(&[o.addr], |_, _| {}));
    let a = {
        let h = h.clone();
        tokio::spawn(async move { h.get("/a", &[("accept-encoding", "gzip")]).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let b = h.get("/a", &[("accept-encoding", "br")]).await;
    assert_eq!(a.await.unwrap().body, "gzip");
    assert_eq!(b.body, "br");
    assert_eq!(o.hits(), 2);
}

#[tokio::test]
async fn large_bodies_stream_through_uncached() {
    let big = "x".repeat(10_000);
    let big2 = big.clone();
    let o = TestOrigin::start(handler(move |parts, _| {
        let mut headers = vec![("cache-control", "max-age=60")];
        if parts.uri.path() == "/chunked" {
            headers.push(("x-test-chunked", "1"));
        }
        reply(200, &headers, big2.clone())
    }))
    .await;
    let h = harness(&[o.addr], |c, _| c.max_object_bytes = 2_000);
    for path in ["/sized", "/chunked"] {
        for _ in 0..2 {
            let got = h.get(path, &[]).await;
            assert_eq!(got.status, 200);
            assert_eq!(got.body.len(), big.len(), "{path}");
            assert_eq!(got.cache_status(), "test-node; fwd=miss; fwd-status=200");
        }
    }
    assert_eq!(o.hits(), 4);
    assert_eq!(
        h.proxy
            .metrics
            .streamed_uncached
            .load(std::sync::atomic::Ordering::Relaxed),
        4
    );
    // A chunked body under the limit is cached.
    let o = TestOrigin::start(handler(|_, _| {
        reply(
            200,
            &[("cache-control", "max-age=60"), ("x-test-chunked", "1")],
            "small",
        )
    }))
    .await;
    let h = harness(&[o.addr], |c, _| c.max_object_bytes = 2_000);
    h.get("/a", &[]).await;
    assert_eq!(h.get("/a", &[]).await.body, "small");
    assert_eq!(o.hits(), 1);
}

#[tokio::test]
async fn unsafe_methods_pass_through_and_invalidate() {
    let o = TestOrigin::start(handler(|parts, body| {
        if parts.method == "POST" {
            reply(
                201,
                &[
                    ("location", "/items/2"),
                    ("content-location", "http://example.com/items/3"),
                ],
                format!("got {}", body.len()),
            )
        } else {
            reply(
                200,
                &[("cache-control", "max-age=60")],
                parts.uri.path().to_string(),
            )
        }
    }))
    .await;
    let h = harness(&[o.addr], |_, _| {});
    for p in ["/items", "/items/2", "/items/3", "/other"] {
        h.get(p, &[]).await;
    }
    assert_eq!(o.hits(), 4);
    let posted = h
        .send(
            "POST",
            "/items",
            &[("content-type", "text/plain")],
            "payload",
        )
        .await;
    assert_eq!(posted.status, 201);
    assert_eq!(posted.body, "got 7");
    assert_eq!(
        posted.cache_status(),
        "test-node; fwd=method; fwd-status=201"
    );
    assert_eq!(o.last().body, "payload");
    for p in ["/items", "/items/2", "/items/3"] {
        assert!(
            h.get(p, &[]).await.cache_status().contains("fwd=miss"),
            "{p} was not invalidated"
        );
    }
    assert!(h.get("/other", &[]).await.cache_status().contains("hit"));
    h.send("POST", "/items", &[("x-forwarded-for", "198.51.100.7")], "")
        .await;
    assert_eq!(
        o.last().headers["x-forwarded-for"],
        "198.51.100.7, 192.0.2.1"
    );
}

#[tokio::test]
async fn failed_unsafe_requests_do_not_invalidate() {
    let o = TestOrigin::start(handler(|parts, _| {
        if parts.method == "DELETE" {
            reply(500, &[], "")
        } else {
            reply(200, &[("cache-control", "max-age=60")], "x")
        }
    }))
    .await;
    let h = harness(&[o.addr], |_, _| {});
    h.get("/a", &[]).await;
    assert_eq!(h.send("DELETE", "/a", &[], "").await.status, 500);
    assert!(h.get("/a", &[]).await.cache_status().contains("hit"));
}

#[tokio::test]
async fn invalidation_ignores_other_hosts() {
    let o = TestOrigin::start(handler(|parts, _| {
        if parts.method == "PUT" {
            reply(
                204,
                &[
                    ("location", "http://elsewhere.example/a"),
                    ("content-location", "relative"),
                ],
                "",
            )
        } else {
            reply(200, &[("cache-control", "max-age=60")], "x")
        }
    }))
    .await;
    let h = harness(&[o.addr], |_, _| {});
    h.get("/a", &[]).await;
    h.get("/a", &[("host", "elsewhere.example")]).await;
    h.send("PUT", "/b", &[], "").await;
    assert!(h.get("/a", &[]).await.cache_status().contains("hit"));
    assert!(
        h.get("/a", &[("host", "elsewhere.example")])
            .await
            .cache_status()
            .contains("hit")
    );
}

#[tokio::test]
async fn head_requests() {
    let o = origin_with(&[("cache-control", "max-age=60")], "body").await;
    let h = harness(&[o.addr], |_, _| {});
    let miss = h.send("HEAD", "/a", &[], "").await;
    assert_eq!(miss.status, 200);
    assert!(miss.body.is_empty());
    assert!(miss.cache_status().contains("fwd=miss"));
    assert_eq!(o.last().method, "HEAD");
    h.get("/a", &[]).await;
    let hit = h.send("HEAD", "/a", &[], "").await;
    assert!(hit.cache_status().contains("hit"));
    assert_eq!(hit.header("content-length"), "4");
    assert!(hit.body.is_empty());
    assert_eq!(o.hits(), 2);
}

#[tokio::test]
async fn request_directives() {
    let o = origin_with(&[("cache-control", "max-age=60")], "x").await;
    let h = harness(&[o.addr], |_, _| {});
    let got = h.get("/a", &[("cache-control", "only-if-cached")]).await;
    assert_eq!(got.status, 504);
    assert_eq!(o.hits(), 0);
    let got = h.get("/a", &[("cache-control", "no-store")]).await;
    assert_eq!(got.cache_status(), "test-node; fwd=request; fwd-status=200");
    let got = h
        .send("HEAD", "/a", &[("cache-control", "no-store")], "")
        .await;
    assert!(got.cache_status().contains("fwd=request"));
    h.get("/a", &[]).await;
    assert!(
        h.get("/a", &[("cache-control", "only-if-cached")])
            .await
            .cache_status()
            .contains("hit")
    );
    assert!(
        h.get("/a", &[("cache-control", "no-cache")])
            .await
            .cache_status()
            .contains("fwd=miss")
    );
    assert_eq!(o.hits(), 4);
}

#[tokio::test]
async fn max_stale_serves_a_stale_copy() {
    let o = origin_with(&[("cache-control", "max-age=10")], "x").await;
    let h = harness(&[o.addr], |_, _| {});
    h.get("/a", &[]).await;
    h.clock.advance(Duration::from_secs(15));
    let got = h.get("/a", &[("cache-control", "max-stale=10")]).await;
    assert!(
        got.cache_status().contains("detail=max-stale"),
        "{}",
        got.cache_status()
    );
    let got = h.get("/a", &[]).await;
    assert!(got.cache_status().contains("fwd=miss"));
    assert_eq!(o.hits(), 2);
}

#[tokio::test]
async fn malformed_requests_are_refused() {
    let o = origin_with(&[], "x").await;
    let h = harness(&[o.addr], |_, p| p.max_uri_bytes = 100);
    let long = format!("/{}", "a".repeat(200));
    assert_eq!(h.get(&long, &[]).await.status, 414);
    assert_eq!(
        h.get("/", &[("via", "1.1 proxy-a, 1.1 test-node")])
            .await
            .status,
        508
    );
    let req = http::Request::builder()
        .uri("/no-host")
        .body(http_body_util::Full::new(bytes::Bytes::new()))
        .unwrap();
    let resp = h.proxy.handle(req, "192.0.2.1:1".parse().unwrap()).await;
    assert_eq!(resp.status(), 400);
    assert_eq!(o.hits(), 0);
}

#[tokio::test]
async fn oversized_request_bodies_are_refused() {
    let o = origin_with(&[], "x").await;
    let h = harness(&[o.addr], |_, p| p.max_request_body = 10);
    let got = h
        .send("POST", "/a", &[("content-length", "11")], "0123456789A")
        .await;
    assert_eq!(got.status, 413);
    assert_eq!(o.hits(), 0);
    let got = h
        .send(
            "POST",
            "/a",
            &[("transfer-encoding", "chunked")],
            "0123456789ABCDEF",
        )
        .await;
    assert_eq!(got.status, 413);
    let got = h.send("POST", "/a", &[], "small").await;
    assert_eq!(got.status, 200);
}

#[tokio::test]
async fn origin_failures_map_to_gateway_errors() {
    let h = harness(&[closed_port().await], |_, _| {});
    let got = h.get("/a", &[]).await;
    assert_eq!(got.status, 502);
    assert_eq!(got.cache_status(), "test-node; fwd=miss; detail=connect");
    assert_eq!(got.header("cache-control"), "no-store");
    assert_eq!(h.send("POST", "/a", &[], "x").await.status, 502);
    let (addr, _task) = black_hole().await;
    let h = harness(&[addr], |_, _| {});
    let got = h.get("/a", &[]).await;
    assert_eq!(got.status, 504);
    assert!(got.cache_status().contains("detail=timeout"));
}

#[tokio::test]
async fn a_stalled_body_is_a_timeout() {
    let (addr, _task) = stalled_body().await;
    let h = harness(&[addr], |_, p| p.body_timeout = Duration::from_millis(200));
    let got = h.get("/a", &[]).await;
    assert_eq!(got.status, 504);
    assert!(got.cache_status().contains("detail=timeout"));
    assert!(h.proxy.cache.memory().is_empty());
}

#[tokio::test]
async fn non_200_responses_are_cached_when_allowed() {
    let o = TestOrigin::start(handler(|_, _| {
        reply(404, &[("cache-control", "max-age=60")], "missing")
    }))
    .await;
    let h = harness(&[o.addr], |_, _| {});
    assert_eq!(h.get("/gone", &[]).await.status, 404);
    let got = h.get("/gone", &[]).await;
    assert_eq!(got.status, 404);
    assert!(got.cache_status().contains("hit"));
    assert_eq!(o.hits(), 1);
}

#[tokio::test]
async fn invalid_node_names_fall_back_to_a_token() {
    let o = origin_with(&[("cache-control", "max-age=60")], "x").await;
    let h = harness(&[o.addr], |_, p| p.node_id = "bad name".into());
    assert!(
        h.get("/a", &[])
            .await
            .cache_status()
            .starts_with("cachet; ")
    );
    assert_eq!(h.proxy.in_flight(), 0);
}
