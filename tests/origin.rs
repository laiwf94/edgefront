mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use cachet::origin::{HealthSettings, Origin, OriginError, OriginPool, Outbound, client};
use cachet::respond::full;
use common::{TestOrigin, black_hole, closed_port, eventually, garbage_server, handler, reply};
use http::uri::PathAndQuery;
use http::{HeaderMap, Method};

fn health() -> HealthSettings {
    HealthSettings {
        interval: Duration::from_millis(30),
        timeout: Duration::from_millis(200),
        rise: 2,
        fall: 2,
        ..HealthSettings::default()
    }
}

fn pool(addrs: &[std::net::SocketAddr]) -> Arc<OriginPool> {
    let origins = addrs
        .iter()
        .enumerate()
        .map(|(i, a)| {
            Arc::new(Origin::new(
                &format!("o{i}"),
                a.to_string().parse().unwrap(),
            ))
        })
        .collect();
    Arc::new(OriginPool::new(
        origins,
        client(Duration::from_millis(200)),
        Duration::from_millis(300),
        health(),
    ))
}

fn get(path: &'static str) -> Outbound {
    Outbound {
        method: Method::GET,
        path: PathAndQuery::from_static(path),
        headers: HeaderMap::new(),
        body: None,
    }
}

async fn named(tag: &'static str) -> TestOrigin {
    TestOrigin::start(handler(move |_, _| reply(200, &[], tag))).await
}

#[tokio::test]
async fn uses_the_primary_when_it_is_up() {
    let (a, b) = (named("a").await, named("b").await);
    let p = pool(&[a.addr, b.addr]);
    let (resp, origin) = p.send(get("/x")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(origin.name, "o0");
    assert_eq!((a.hits(), b.hits()), (1, 0));
    assert_eq!(a.last().uri.path(), "/x");
}

#[tokio::test]
async fn fails_over_when_the_primary_refuses() {
    let down = closed_port().await;
    let b = named("b").await;
    let p = pool(&[down, b.addr]);
    for _ in 0..3 {
        let (_, origin) = p.send(get("/")).await.unwrap();
        assert_eq!(origin.name, "o1");
    }
    // Two connect failures (fall = 2) mark the primary down, so the third
    // request went straight to the secondary.
    let primary = &p.origins()[0];
    assert!(!primary.is_healthy());
    assert_eq!(primary.errors.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert_eq!(b.hits(), 3);
}

#[tokio::test]
async fn requests_with_a_body_are_not_retried() {
    let down = closed_port().await;
    let b = named("b").await;
    let p = pool(&[down, b.addr]);
    let mut out = get("/submit");
    out.method = Method::POST;
    out.body = Some(full("payload"));
    assert_eq!(p.send(out).await.unwrap_err(), OriginError::Connect);
    assert_eq!(b.hits(), 0);
}

#[tokio::test]
async fn a_hung_origin_times_out() {
    let (addr, _task) = black_hole().await;
    let p = pool(&[addr]);
    let start = Instant::now();
    assert_eq!(p.send(get("/")).await.unwrap_err(), OriginError::Timeout);
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn a_slow_primary_fails_over_to_the_secondary() {
    let (a, b) = (named("a").await, named("b").await);
    a.set_delay(Duration::from_secs(5));
    let p = pool(&[a.addr, b.addr]);
    let (_, origin) = p.send(get("/")).await.unwrap();
    assert_eq!(origin.name, "o1");
}

#[tokio::test]
async fn garbage_is_a_protocol_error() {
    let (addr, _task) = garbage_server().await;
    let p = pool(&[addr]);
    assert_eq!(p.send(get("/")).await.unwrap_err(), OriginError::Protocol);
    // Protocol errors do not count against health.
    assert!(p.origins()[0].is_healthy());
}

#[tokio::test]
async fn every_origin_down_still_tries_them_all() {
    let (a, b) = (closed_port().await, closed_port().await);
    let p = pool(&[a, b]);
    for _ in 0..3 {
        assert_eq!(p.send(get("/")).await.unwrap_err(), OriginError::Connect);
    }
    assert!(p.origins().iter().all(|o| !o.is_healthy()));
    assert_eq!(
        p.origins()[1]
            .requests
            .load(std::sync::atomic::Ordering::Relaxed),
        3
    );
}

#[tokio::test]
async fn health_checks_take_an_origin_out_and_bring_it_back() {
    let (a, b) = (named("a").await, named("b").await);
    let p = pool(&[a.addr, b.addr]);
    let checker = p.spawn_health_checks();
    a.stop();
    eventually("primary marked down", || !p.origins()[0].is_healthy()).await;
    let (_, origin) = p.send(get("/")).await.unwrap();
    assert_eq!(origin.name, "o1");
    a.restart().await;
    eventually("primary healthy again", || p.origins()[0].is_healthy()).await;
    let (_, origin) = p.send(get("/")).await.unwrap();
    assert_eq!(origin.name, "o0");
    checker.abort();
    assert_eq!(
        a.seen()
            .iter()
            .filter(|s| s.uri.path() == "/healthz")
            .count(),
        0
    );
}

#[tokio::test]
async fn unhealthy_status_from_the_probe_marks_down() {
    let a = TestOrigin::start(handler(|parts, _| {
        if parts.uri.path() == "/healthz" {
            reply(503, &[], "")
        } else {
            reply(200, &[], "ok")
        }
    }))
    .await;
    let p = pool(&[a.addr]);
    p.check_all().await;
    p.check_all().await;
    assert!(!p.origins()[0].is_healthy());
    // Real traffic that succeeds brings it back after `rise` successes.
    p.send(get("/")).await.unwrap();
    p.send(get("/")).await.unwrap();
    assert!(p.origins()[0].is_healthy());
}
