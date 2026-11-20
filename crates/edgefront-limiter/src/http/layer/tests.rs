use std::sync::atomic::AtomicUsize;

use edgefront_core::body::text;
use edgefront_core::filter::run;
use http::Request;

use super::*;
use crate::clock::ManualClock;
use crate::config::http_limits::{HttpLimitsSection, build};

const SEC: u64 = 1_000_000_000;

fn limiter(toml_text: &str) -> (Arc<HttpLimiter>, Arc<ManualClock>) {
    let section: HttpLimitsSection = toml::from_str(toml_text).unwrap();
    let rules = build(&section.http_limits).unwrap();
    let clock = Arc::new(ManualClock::new(1_000 * SEC));
    let l = Arc::new(HttpLimiter::new(rules, clock.clone()).unwrap());
    (l, clock)
}

/// The upstream answers with the status in an `x-status` header, 200
/// without one, and counts its calls.
#[derive(Clone, Default)]
struct Upstream {
    calls: Arc<AtomicUsize>,
}

impl Upstream {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn send(
        &self,
    ) -> impl FnMut(Request<()>) -> BoxFuture<'static, (Response<Body>, Delivery)> + Send + 'static
    {
        let calls = self.calls.clone();
        move |req: Request<()>| {
            calls.fetch_add(1, Ordering::Relaxed);
            let status = req
                .headers()
                .get("x-status")
                .and_then(|v| v.to_str().ok()?.parse::<u16>().ok())
                .unwrap_or(200);
            Box::pin(async move {
                if status == 502 {
                    return (
                        text(StatusCode::BAD_GATEWAY, "x"),
                        Delivery::Generated("connect"),
                    );
                }
                let mut r = Response::new(full("ok"));
                *r.status_mut() = StatusCode::from_u16(status).unwrap();
                (r, Delivery::Upstream)
            })
        }
    }
}

async fn send(
    filters: &RouteFilters,
    route: usize,
    up: &Upstream,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
) -> Response<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(target)
        .header("host", "app.test");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    run(filters.get(route), route, b.body(()).unwrap(), up.send()).await
}

fn hv<'a>(r: &'a Response<Body>, name: &str) -> &'a str {
    r.headers().get(name).map_or("", |v| v.to_str().unwrap())
}

#[tokio::test]
async fn a_key_over_its_limit_is_refused_with_exact_headers() {
    let (l, clock) = limiter(
        r#"
        [[http_limits]]
        name = "api"
        rate = "2/s burst 3"
        key = ["path", "header:x-api-key"]
        "#,
    );
    let f = l.route_filters(1).unwrap();
    let up = Upstream::default();
    for left in [2, 1, 0] {
        let r = send(&f, 0, &up, "GET", "/v1", &[("x-api-key", "a")]).await;
        assert_eq!(r.status(), 200);
        assert_eq!(hv(&r, "ratelimit-policy"), "\"api\";q=3;w=2");
        assert!(
            hv(&r, "ratelimit").starts_with(&format!("\"api\";r={left}")),
            "{}",
            hv(&r, "ratelimit")
        );
    }
    let r = send(&f, 0, &up, "GET", "/v1", &[("x-api-key", "a")]).await;
    assert_eq!(r.status(), 429);
    assert_eq!(hv(&r, "retry-after"), "1");
    assert_eq!(hv(&r, "ratelimit"), "\"api\";r=0;t=0;edgefront-ms=500");
    assert_eq!(hv(&r, "content-type"), "text/plain; charset=utf-8");
    assert_eq!(up.calls(), 3, "the refusal never reached the upstream");
    // Another key, and the same key elsewhere, are their own buckets.
    assert_eq!(
        send(&f, 0, &up, "GET", "/v1", &[("x-api-key", "b")])
            .await
            .status(),
        200
    );
    assert_eq!(
        send(&f, 0, &up, "GET", "/v2", &[("x-api-key", "a")])
            .await
            .status(),
        200
    );
    // No key: the rule does not apply.
    assert_eq!(send(&f, 0, &up, "GET", "/v1", &[]).await.status(), 200);
    // 499 ms later still refused; at 500 ms admitted.
    clock.advance(499_000_000);
    assert_eq!(
        send(&f, 0, &up, "GET", "/v1", &[("x-api-key", "a")])
            .await
            .status(),
        429
    );
    clock.advance(1_000_000);
    assert_eq!(
        send(&f, 0, &up, "GET", "/v1", &[("x-api-key", "a")])
            .await
            .status(),
        200
    );
    let s = &l.stats()[0];
    assert_eq!((s.matched, s.allowed, s.refused, s.keys), (8, 6, 2, 3));
}

#[tokio::test]
async fn failed_logins_are_counted_and_successful_ones_are_not() {
    let (l, _clock) = limiter(
        r#"
        [[http_limits]]
        name = "login"
        rate = "5/min burst 5"
        key = ["header:x-user"]
        count = [401, 403]
        [http_limits.match]
        path = "/login"
        methods = ["POST"]
        "#,
    );
    let f = l.route_filters(1).unwrap();
    let up = Upstream::default();
    // A user whose logins succeed is never limited.
    for _ in 0..100 {
        let r = send(
            &f,
            0,
            &up,
            "POST",
            "/login",
            &[("x-user", "alice"), ("x-status", "200")],
        )
        .await;
        assert_eq!(r.status(), 200);
        assert!(
            hv(&r, "ratelimit").starts_with("\"login\";r=5"),
            "{}",
            hv(&r, "ratelimit")
        );
    }
    // A guesser gets five failures, then is refused without reaching
    // the upstream.
    for n in 0..5 {
        let r = send(
            &f,
            0,
            &up,
            "POST",
            "/login",
            &[("x-user", "bob"), ("x-status", "401")],
        )
        .await;
        assert_eq!(r.status(), 401, "guess {n}");
    }
    let before = up.calls();
    let r = send(
        &f,
        0,
        &up,
        "POST",
        "/login",
        &[("x-user", "bob"), ("x-status", "200")],
    )
    .await;
    assert_eq!(r.status(), 429);
    assert_eq!(hv(&r, "retry-after"), "12");
    assert_eq!(up.calls(), before);
    // A 403 counts too, a 502 the proxy wrote does not.
    for _ in 0..4 {
        send(
            &f,
            0,
            &up,
            "POST",
            "/login",
            &[("x-user", "carol"), ("x-status", "403")],
        )
        .await;
    }
    for _ in 0..10 {
        send(
            &f,
            0,
            &up,
            "POST",
            "/login",
            &[("x-user", "carol"), ("x-status", "502")],
        )
        .await;
    }
    let r = send(
        &f,
        0,
        &up,
        "POST",
        "/login",
        &[("x-user", "carol"), ("x-status", "200")],
    )
    .await;
    assert_eq!(r.status(), 200, "four counted, one left");
    // GETs and other paths are not the rule's.
    for _ in 0..10 {
        let r = send(&f, 0, &up, "GET", "/login", &[("x-user", "bob")]).await;
        assert_eq!(r.status(), 200);
        assert_eq!(hv(&r, "ratelimit"), "");
    }
    let s = &l.stats()[0];
    assert_eq!(s.refunded, 100 + 10 + 1);
}

#[tokio::test]
async fn a_refusal_by_one_rule_spends_nothing_in_another() {
    let (l, _clock) = limiter(
        r#"
        [[http_limits]]
        name = "per-key"
        rate = "100/s"
        key = ["header:x-key"]

        [[http_limits]]
        name = "per-path"
        rate = "1/min burst 1"
        key = ["path"]
        "#,
    );
    let f = l.route_filters(1).unwrap();
    let up = Upstream::default();
    let r = send(&f, 0, &up, "GET", "/a", &[("x-key", "k")]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        hv(&r, "ratelimit-policy"),
        "\"per-key\";q=100;w=1, \"per-path\";q=1;w=60"
    );
    assert!(hv(&r, "ratelimit").contains("\"per-key\";r=99"));
    for _ in 0..50 {
        let r = send(&f, 0, &up, "GET", "/a", &[("x-key", "k")]).await;
        assert_eq!(r.status(), 429);
        assert_eq!(hv(&r, "ratelimit-policy"), "\"per-path\";q=1;w=60");
    }
    // per-key gave back every token the refused requests took.
    let r = send(&f, 0, &up, "GET", "/b", &[("x-key", "k")]).await;
    assert!(
        hv(&r, "ratelimit").starts_with("\"per-key\";r=98"),
        "{}",
        hv(&r, "ratelimit")
    );
}

#[tokio::test]
async fn the_log_action_lets_requests_through_and_counts_them() {
    let (l, _clock) = limiter(
        r#"
        [[http_limits]]
        name = "watch"
        rate = "1/min burst 2"
        action = "log"
        headers = false
        "#,
    );
    let f = l.route_filters(1).unwrap();
    let up = Upstream::default();
    for _ in 0..10 {
        let r = send(&f, 0, &up, "GET", "/", &[]).await;
        assert_eq!(r.status(), 200);
        assert_eq!(hv(&r, "ratelimit"), "", "headers off");
    }
    let s = &l.stats()[0];
    assert_eq!((s.allowed, s.logged, s.refused), (2, 8, 0));
}

#[tokio::test(start_paused = true)]
async fn the_delay_action_holds_requests_until_their_token_is_due() {
    let (l, clock) = limiter(
        r#"
        [[http_limits]]
        name = "slow"
        rate = "10/s burst 1"
        action = "delay"
        max_delay = "250ms"
        "#,
    );
    let f = l.route_filters(1).unwrap();
    let up = Upstream::default();
    assert_eq!(send(&f, 0, &up, "GET", "/", &[]).await.status(), 200);
    // Each later request books the next token: 100, 200 ms; the third
    // would wait 300 ms and is refused.
    let start = tokio::time::Instant::now();
    let r = send(&f, 0, &up, "GET", "/", &[]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(start.elapsed(), Duration::from_millis(100));
    let start = tokio::time::Instant::now();
    assert_eq!(send(&f, 0, &up, "GET", "/", &[]).await.status(), 200);
    assert_eq!(
        start.elapsed(),
        Duration::from_millis(200),
        "the clock did not move: 2 booked"
    );
    let r = send(&f, 0, &up, "GET", "/", &[]).await;
    assert_eq!(r.status(), 429);
    assert_eq!(hv(&r, "ratelimit"), "\"slow\";r=0;t=0;edgefront-ms=300");
    clock.advance(SEC);
    assert_eq!(send(&f, 0, &up, "GET", "/", &[]).await.status(), 200);
    let s = &l.stats()[0];
    assert_eq!((s.delayed, s.refused, s.allowed), (2, 1, 4));
}

#[tokio::test]
async fn rules_attach_to_their_routes() {
    let (l, _clock) = limiter(
        r#"
        [[http_limits]]
        name = "only-one"
        routes = [1]
        rate = "1/min burst 1"
        "#,
    );
    let f = l.route_filters(3).unwrap();
    assert_eq!(f.filtered(), 1);
    assert!(f.get(0).is_none() && f.get(1).is_some());
    let up = Upstream::default();
    for _ in 0..3 {
        assert_eq!(send(&f, 0, &up, "GET", "/", &[]).await.status(), 200);
    }
    assert_eq!(send(&f, 1, &up, "GET", "/", &[]).await.status(), 200);
    assert_eq!(send(&f, 1, &up, "GET", "/", &[]).await.status(), 429);
    assert_eq!(
        l.route_filters(1).unwrap_err(),
        "http limit only-one: route 1 does not exist; the proxy has 1 routes"
    );
    let many: String = (0..17)
        .map(|i| format!("[[http_limits]]\nname = \"r{i}\"\nrate = \"1/s\"\n"))
        .collect();
    let (l, _) = limiter(&many);
    assert_eq!(
        l.route_filters(1).unwrap_err(),
        "route 0 has 17 http limits; at most 16 may apply to one route"
    );
    assert_eq!(format!("{:?}", l), "HttpLimiter { rules: 17 }");
    assert_eq!(l.rules().count(), 17);
}

#[tokio::test]
async fn the_refusal_is_configurable_and_a_full_table_can_deny() {
    let (l, _clock) = limiter(
        r#"
        [[http_limits]]
        name = "tight"
        rate = "1/min burst 1"
        key = ["header:x-k"]
        table_size = 1
        table_full = "deny"
        headers = false
        [http_limits.response]
        status = 503
        body = "{\"error\":\"slow down\"}"
        content_type = "application/json"
        "#,
    );
    let f = l.route_filters(1).unwrap();
    let up = Upstream::default();
    let r = send(&f, 0, &up, "GET", "/", &[("x-k", "a")]).await;
    assert_eq!(r.status(), 200);
    assert!(r.headers().get("ratelimit").is_none());
    // The one slot is held by a key in debt: a new key is refused.
    let r = send(&f, 0, &up, "GET", "/", &[("x-k", "b")]).await;
    assert_eq!(r.status(), 503);
    assert_eq!(hv(&r, "content-type"), "application/json");
    assert_eq!(hv(&r, "retry-after"), "60");
    assert!(r.headers().get("ratelimit").is_none());
    use http_body_util::BodyExt;
    let body = r.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, "{\"error\":\"slow down\"}");
    let s = &l.stats()[0];
    assert_eq!((s.table_full, s.refused), (1, 1));
    // A rule without headers or counting needs no deliver phase.
    let d = l
        .decide(
            &[0],
            &Method::GET,
            &"/".parse().unwrap(),
            &http::HeaderMap::new(),
        )
        .await;
    assert_eq!(
        format!("{d:?}"),
        "Forward(None)",
        "no x-k: the rule does not apply"
    );
}

#[test]
fn the_match_reads_host_path_and_method() {
    let m = Match {
        host: Some(normal_host("App.Test:8080")),
        path_prefix: Some("/api".into()),
        path: None,
        methods: vec![Method::POST],
    };
    let check = |method: Method, target: &str, host: &str| {
        let uri: http::Uri = target.parse().unwrap();
        let mut h = http::HeaderMap::new();
        h.insert("host", HeaderValue::from_str(host).unwrap());
        let t = RequestTarget::parse(uri.path_and_query().unwrap().as_str().as_bytes()).unwrap();
        m.matches(&View {
            method: &method,
            uri: &uri,
            headers: &h,
            path: t.clean_path(),
        })
    };
    assert!(check(Method::POST, "/api/x", "app.test"));
    assert!(check(Method::POST, "/api", "APP.test:1"));
    assert!(!check(Method::GET, "/api/x", "app.test"));
    assert!(!check(Method::POST, "/apix", "app.test"));
    assert!(!check(Method::POST, "/api/x", "other.test"));
    assert!(Match::default().matches(&View {
        method: &Method::GET,
        uri: &"/".parse().unwrap(),
        headers: &http::HeaderMap::new(),
        path: b"/",
    }));
    assert_eq!(Action::Delay(Duration::ZERO).name(), "delay");
    assert_eq!(Action::Log.name(), "log");
    assert_eq!(Action::Block.name(), "block");
}

#[tokio::test]
async fn a_key_over_its_limit_for_n_windows_is_boxed_then_banned() {
    let (l, clock) = limiter(
        r#"
        [[http_limits]]
        name = "api"
        rate = "1/s burst 1"
        key = ["header:x-key"]
        [http_limits.penalty]
        windows = 2
        window = "1s"
        timeout = "10s"
        ban_after = 2
        ban_for = "1h"
        "#,
    );
    let f = l.route_filters(1).unwrap();
    let up = Upstream::default();
    let k = [("x-key", "abuser")];
    // Second 0: one admitted, the rest refused; second 1 the same.
    for sec in 0..2 {
        assert_eq!(
            send(&f, 0, &up, "GET", "/", &k).await.status(),
            200,
            "{sec}"
        );
        assert_eq!(send(&f, 0, &up, "GET", "/", &k).await.status(), 429);
        clock.advance(SEC);
    }
    // Boxed at the end of second 1 for 10 s: refused without the table,
    // even once its bucket has refilled.
    let r = send(&f, 0, &up, "GET", "/", &k).await;
    assert_eq!(r.status(), 429);
    assert_eq!(hv(&r, "retry-after"), "9");
    assert_eq!(l.stats()[0].boxed, 1);
    let calls = up.calls();
    clock.advance(8 * SEC);
    assert_eq!(send(&f, 0, &up, "GET", "/", &k).await.status(), 429);
    assert_eq!(up.calls(), calls);
    // Released: admitted again.
    clock.advance(SEC);
    assert_eq!(send(&f, 0, &up, "GET", "/", &k).await.status(), 200);
    // A second offence bans the key for an hour.
    assert_eq!(send(&f, 0, &up, "GET", "/", &k).await.status(), 429);
    clock.advance(SEC);
    send(&f, 0, &up, "GET", "/", &k).await;
    assert_eq!(send(&f, 0, &up, "GET", "/", &k).await.status(), 429);
    assert_eq!(l.bans().len(), 1);
    clock.advance(30 * 60 * SEC);
    let r = send(&f, 0, &up, "GET", "/", &k).await;
    assert_eq!(r.status(), 429);
    assert_eq!(hv(&r, "retry-after"), "1800");
    assert!(l.stats()[0].banned >= 1);
    // Others were never affected, and a manual unban lifts it.
    assert_eq!(
        send(&f, 0, &up, "GET", "/", &[("x-key", "fine")])
            .await
            .status(),
        200
    );
    let bk = l.bans().list()[0].0.clone();
    assert!(bk.starts_with(b"api\0"));
    assert!(l.bans().unban(&bk));
    assert_eq!(send(&f, 0, &up, "GET", "/", &k).await.status(), 200);
    // A rebuilt limiter keeps the bans it is given.
    let rebuilt = HttpLimiter::new(l.rules().cloned().collect(), clock.clone())
        .unwrap()
        .with_bans(Arc::clone(l.bans()));
    assert!(Arc::ptr_eq(rebuilt.bans(), l.bans()));
}
