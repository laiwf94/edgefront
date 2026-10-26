mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{TestOrigin, eventually, handler, harness, reply};

const SECOND: Duration = Duration::from_secs(1);

/// An origin serving one versioned document with an entity tag, answering
/// matching If-None-Match with 304.
async fn versioned(cache_control: &'static str) -> (TestOrigin, Arc<Mutex<String>>) {
    let version = Arc::new(Mutex::new("v1".to_string()));
    let v = version.clone();
    let o = TestOrigin::start(handler(move |parts, _| {
        let current = v.lock().unwrap().clone();
        let etag = format!("\"{current}\"");
        let matches = parts
            .headers
            .get("if-none-match")
            .is_some_and(|inm| inm.to_str().unwrap() == etag);
        if matches {
            reply(
                304,
                &[
                    ("cache-control", cache_control),
                    ("etag", &etag),
                    ("x-refreshed", "yes"),
                ],
                "",
            )
        } else {
            reply(
                200,
                &[("cache-control", cache_control), ("etag", &etag)],
                format!("body {current}"),
            )
        }
    }))
    .await;
    (o, version)
}

#[tokio::test]
async fn stale_object_is_revalidated_with_its_entity_tag() {
    let (o, _) = versioned("max-age=10").await;
    let h = harness(&[o.addr], |_, _| {});
    o.use_clock(&h.clock);
    assert_eq!(h.get("/doc", &[]).await.body, "body v1");
    h.clock.advance(15 * SECOND);
    let got = h.get("/doc", &[]).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.body, "body v1");
    assert_eq!(got.header("x-refreshed"), "yes");
    assert!(
        got.cache_status()
            .starts_with("test-node; fwd=stale; fwd-status=304; stored"),
        "{}",
        got.cache_status()
    );
    assert_eq!(o.last().headers["if-none-match"], "\"v1\"");
    // The 304 made the copy fresh again.
    assert!(h.get("/doc", &[]).await.cache_status().contains("hit"));
    assert_eq!(o.hits(), 2);
    assert_eq!(
        h.proxy
            .metrics
            .requests(cachet::metrics::Outcome::Revalidated),
        1
    );
}

#[tokio::test]
async fn changed_object_replaces_the_stale_copy() {
    let (o, version) = versioned("max-age=10").await;
    let h = harness(&[o.addr], |_, _| {});
    o.use_clock(&h.clock);
    h.get("/doc", &[]).await;
    *version.lock().unwrap() = "v2".into();
    h.clock.advance(15 * SECOND);
    let got = h.get("/doc", &[]).await;
    assert_eq!(got.body, "body v2");
    assert!(
        got.cache_status()
            .contains("fwd=stale; fwd-status=200; stored")
    );
    assert_eq!(h.get("/doc", &[]).await.body, "body v2");
    assert_eq!(o.hits(), 2);
}

#[tokio::test]
async fn last_modified_is_used_without_an_entity_tag() {
    let o = TestOrigin::start(handler(|parts, _| {
        let lm = "Sun, 06 Nov 1994 08:49:37 GMT";
        if parts
            .headers
            .get("if-modified-since")
            .is_some_and(|v| v == lm)
        {
            reply(304, &[("cache-control", "max-age=10")], "")
        } else {
            reply(
                200,
                &[("cache-control", "max-age=10"), ("last-modified", lm)],
                "dated",
            )
        }
    }))
    .await;
    let h = harness(&[o.addr], |_, _| {});
    o.use_clock(&h.clock);
    h.get("/d", &[]).await;
    h.clock.advance(15 * SECOND);
    let got = h.get("/d", &[]).await;
    assert_eq!(got.body, "dated");
    assert!(got.cache_status().contains("fwd-status=304"));
    assert!(o.last().headers.get("if-none-match").is_none());
}

#[tokio::test]
async fn a_304_for_another_representation_triggers_a_full_fetch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let o = TestOrigin::start(handler(move |parts, _| {
        c.fetch_add(1, Ordering::SeqCst);
        if parts.headers.contains_key("if-none-match") {
            reply(304, &[("etag", "\"other\"")], "")
        } else {
            reply(
                200,
                &[("cache-control", "max-age=10"), ("etag", "\"v1\"")],
                "full",
            )
        }
    }))
    .await;
    let h = harness(&[o.addr], |_, _| {});
    o.use_clock(&h.clock);
    h.get("/x", &[]).await;
    h.clock.advance(15 * SECOND);
    let got = h.get("/x", &[]).await;
    assert_eq!(got.body, "full");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert!(o.last().headers.get("if-none-match").is_none());
}

#[tokio::test]
async fn no_cache_responses_are_revalidated_every_time() {
    let (o, _) = versioned("no-cache").await;
    let h = harness(&[o.addr], |_, _| {});
    o.use_clock(&h.clock);
    h.get("/doc", &[]).await;
    for _ in 0..3 {
        let got = h.get("/doc", &[]).await;
        assert_eq!(got.body, "body v1");
        assert!(got.cache_status().contains("fwd-status=304"));
    }
    assert_eq!(o.hits(), 4);
}

#[tokio::test]
async fn client_no_cache_revalidates_a_fresh_copy() {
    let (o, _) = versioned("max-age=60").await;
    let h = harness(&[o.addr], |_, _| {});
    o.use_clock(&h.clock);
    h.get("/doc", &[]).await;
    let got = h.get("/doc", &[("cache-control", "no-cache")]).await;
    assert!(
        got.cache_status()
            .starts_with("test-node; fwd=request; fwd-status=304"),
        "{}",
        got.cache_status()
    );
    assert_eq!(o.last().headers["if-none-match"], "\"v1\"");
    // The client's own conditional is answered from the refreshed copy.
    let got = h
        .get(
            "/doc",
            &[("cache-control", "max-age=0"), ("if-none-match", "\"v1\"")],
        )
        .await;
    assert_eq!(got.status, 304);
}

#[tokio::test]
async fn stale_while_revalidate_serves_at_once_and_refreshes_behind() {
    let (o, version) = versioned("max-age=10, stale-while-revalidate=30").await;
    let h = harness(&[o.addr], |_, _| {});
    o.use_clock(&h.clock);
    h.get("/doc", &[]).await;
    *version.lock().unwrap() = "v2".into();
    o.set_delay(Duration::from_millis(400));
    h.clock.advance(15 * SECOND);
    let start = Instant::now();
    let got = h.get("/doc", &[]).await;
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "stale answer waited for the origin"
    );
    assert_eq!(got.body, "body v1");
    assert!(got.cache_status().contains("detail=stale-while-revalidate"));
    // More stale requests during the refresh do not start more refreshes.
    for _ in 0..5 {
        h.get("/doc", &[]).await;
    }
    eventually("background refresh", || o.hits() == 2).await;
    eventually("refreshed copy", || h.proxy.in_flight() == 0).await;
    let got = h.get("/doc", &[]).await;
    assert_eq!(got.body, "body v2");
    assert!(got.cache_status().contains("hit"));
    assert_eq!(
        h.proxy.metrics.background_refreshes.load(Ordering::Relaxed),
        1
    );
}

#[tokio::test]
async fn past_the_window_the_client_waits_for_revalidation() {
    let (o, _) = versioned("max-age=10, stale-while-revalidate=30").await;
    let h = harness(&[o.addr], |_, _| {});
    o.use_clock(&h.clock);
    h.get("/doc", &[]).await;
    h.clock.advance(50 * SECOND);
    let got = h.get("/doc", &[]).await;
    assert!(got.cache_status().contains("fwd=stale; fwd-status=304"));
}

#[tokio::test]
async fn stale_if_error_covers_an_unreachable_origin() {
    let (o, _) = versioned("max-age=10, stale-if-error=60").await;
    let h = harness(&[o.addr], |_, _| {});
    o.use_clock(&h.clock);
    h.get("/doc", &[]).await;
    o.stop();
    h.clock.advance(15 * SECOND);
    let got = h.get("/doc", &[]).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.body, "body v1");
    assert_eq!(
        got.cache_status(),
        "test-node; hit; ttl=-5; fwd=stale; detail-origin=connect; detail=stale-if-error"
    );
    h.clock.advance(100 * SECOND);
    let got = h.get("/doc", &[]).await;
    assert_eq!(got.status, 502);
    assert_eq!(
        h.proxy
            .metrics
            .requests(cachet::metrics::Outcome::StaleIfError),
        1
    );
}

#[tokio::test]
async fn stale_if_error_covers_origin_5xx() {
    let (o, _) = versioned("max-age=10, stale-if-error=60").await;
    let h = harness(&[o.addr], |_, _| {});
    o.use_clock(&h.clock);
    h.get("/doc", &[]).await;
    o.set_handler(handler(|_, _| {
        reply(503, &[("retry-after", "5")], "down for maintenance")
    }));
    h.clock.advance(15 * SECOND);
    let got = h.get("/doc", &[]).await;
    assert_eq!(got.body, "body v1");
    assert!(
        got.cache_status()
            .contains("fwd-status=503; detail=stale-if-error")
    );
    // A 404 is an answer, not an error: it is passed on.
    o.set_handler(handler(|_, _| reply(404, &[], "gone")));
    let got = h.get("/doc", &[]).await;
    assert_eq!(got.status, 404);
}

#[tokio::test]
async fn stale_if_error_from_the_request() {
    let (o, _) = versioned("max-age=10").await;
    let h = harness(&[o.addr], |_, _| {});
    o.use_clock(&h.clock);
    h.get("/doc", &[]).await;
    o.stop();
    h.clock.advance(15 * SECOND);
    assert_eq!(h.get("/doc", &[]).await.status, 502);
    let got = h
        .get("/doc", &[("cache-control", "stale-if-error=30")])
        .await;
    assert_eq!(got.status, 200);
}

#[tokio::test]
async fn must_revalidate_never_serves_stale_on_error() {
    let (o, _) = versioned("max-age=10, stale-if-error=60, must-revalidate").await;
    let h = harness(&[o.addr], |_, _| {});
    o.use_clock(&h.clock);
    h.get("/doc", &[]).await;
    o.set_handler(handler(|_, _| reply(500, &[], "broken")));
    h.clock.advance(15 * SECOND);
    let got = h.get("/doc", &[]).await;
    assert_eq!(got.status, 500);
    assert_eq!(got.body, "broken");
}

#[tokio::test]
async fn concurrent_revalidations_collapse() {
    let (o, _) = versioned("max-age=10").await;
    let h = Arc::new(harness(&[o.addr], |_, _| {}));
    o.use_clock(&h.clock);
    h.get("/doc", &[]).await;
    h.clock.advance(15 * SECOND);
    o.set_delay(Duration::from_millis(200));
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let h = h.clone();
        tasks.push(tokio::spawn(async move { h.get("/doc", &[]).await }));
    }
    for t in tasks {
        let got = t.await.unwrap();
        assert_eq!(got.body, "body v1");
        assert!(got.cache_status().contains("fwd-status=304"));
    }
    assert_eq!(o.hits(), 2);
}
