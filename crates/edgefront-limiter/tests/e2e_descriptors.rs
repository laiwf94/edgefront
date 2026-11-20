//! End to end, limiter week 7: a request walked through nested
//! descriptors written for envoyproxy/ratelimit, several limits decided
//! together, costs from request bytes, a GraphQL query and an upstream
//! header through the proxy's filter phases, and NGINX-style bursts held
//! until their turn.

use std::sync::Arc;
use std::time::Duration;

use edgefront_core::body::{Body, full};
use edgefront_core::filter::{BoxFuture, Delivery, run};
use edgefront_limiter::burst::{Delay, LimitReq, LimitReqTable, Outcome};
use edgefront_limiter::clock::ManualClock;
use edgefront_limiter::config::http_limits::{HttpLimitsSection, build};
use edgefront_limiter::cost::graphql_cost;
use edgefront_limiter::descriptor::{DescriptorConfig, DescriptorLimiter};
use edgefront_limiter::http::HttpLimiter;
use edgefront_limiter::multi::{Limit, MultiDecision, take_all};
use edgefront_limiter::{Gcra, Rate, Table, TableConfig};
use http::{Request, Response, StatusCode};

const SEC: u64 = 1_000_000_000;

/// An API's limits as an envoyproxy/ratelimit config: per client, a
/// tighter one for the login path with a still tighter one for bots, a
/// GraphQL endpoint charged by complexity, and a health check never
/// limited.
const API: &str = r#"
domain: api
descriptors:
  - key: client
    rate_limit: {unit: second, requests_per_unit: 50}
  - key: path
    value: /login
    rate_limit: {unit: minute, requests_per_unit: 10, name: login}
    descriptors:
      - key: user_agent
        value: "*bot*"
        share_threshold: true
        rate_limit:
          unit: minute
          requests_per_unit: 2
          replaces:
            - name: login
  - key: path
    value: /graphql
    rate_limit: {unit: second, requests_per_unit: 1000}
  - key: path
    value: /health
"#;

#[test]
fn nested_descriptors_with_costs_decide_as_the_service_does() {
    let cfg = DescriptorConfig::load(&[("api.yaml", API)], false).unwrap();
    let l = DescriptorLimiter::new(cfg, 1 << 16);
    let t = 1_800_000_000;
    let client: &[(&str, &str)] = &[("client", "10.0.0.1")];
    let login: &[(&str, &str)] = &[("path", "/login")];
    // A login carries both descriptors: ten logins a minute pass.
    for n in 0..10 {
        assert!(
            !l.should_rate_limit("api", &[client, login], 1, t)
                .over_limit,
            "{n}"
        );
    }
    let r = l.should_rate_limit("api", &[client, login], 1, t);
    assert!(r.over_limit);
    assert!(!r.statuses[0].over_limit && r.statuses[1].over_limit);
    assert_eq!(
        r.statuses[1].limit.as_ref().unwrap().name.as_deref(),
        Some("login")
    );
    // Bots share two a minute between them, and their limit replaces the
    // login one in a request carrying both.
    let bot1: &[(&str, &str)] = &[("path", "/login"), ("user_agent", "evilbot/1")];
    let bot2: &[(&str, &str)] = &[("path", "/login"), ("user_agent", "a-bot-b")];
    let t2 = t + 60;
    assert!(!l.should_rate_limit("api", &[login, bot1], 1, t2).over_limit);
    assert!(!l.should_rate_limit("api", &[login, bot2], 1, t2).over_limit);
    let r = l.should_rate_limit("api", &[login, bot1], 1, t2);
    assert!(r.over_limit);
    assert!(r.statuses[0].limit.is_none(), "replaced by the bot limit");
    // GraphQL charged by complexity: a query of 202 points is 202 hits.
    let q = "{ repository(owner: \"a\", name: \"b\") { issues(first: 50) { nodes { title author { login } } } } }";
    let cost = graphql_cost(q);
    assert_eq!(cost, 202);
    let gql: &[(&str, &str)] = &[("path", "/graphql")];
    for _ in 0..4 {
        assert!(!l.should_rate_limit("api", &[gql], cost, t).over_limit);
    }
    let r = l.should_rate_limit("api", &[gql], cost, t);
    assert!(r.over_limit, "5 x 202 passes 1000");
    // The health check is listed without a limit.
    let health: &[(&str, &str)] = &[("path", "/health")];
    let r = l.should_rate_limit("api", &[health], 1_000_000, t);
    assert!(!r.over_limit && r.statuses[0].limit.is_none());
}

#[test]
fn a_request_under_five_limits_takes_from_all_or_none() {
    let global = Gcra::new(&Rate::new(1000, SEC, 1000)).unwrap();
    let route = Gcra::new(&Rate::new(100, SEC, 100)).unwrap();
    let tenant = Gcra::new(&Rate::new(10, SEC, 5)).unwrap();
    let per_client = Table::new(&TableConfig::new(1024, Rate::new(1, SEC, 20))).unwrap();
    let per_key = Table::new(&TableConfig::new(1024, Rate::new(1, SEC, 50))).unwrap();
    let now = 10 * SEC;
    let limits = [
        Limit::Shared {
            gcra: &tenant,
            cost: 1,
        },
        Limit::Keyed {
            table: &per_client,
            key: b"10.0.0.1",
            cost: 1,
        },
        Limit::Keyed {
            table: &per_key,
            key: b"k1",
            cost: 1,
        },
        Limit::Shared {
            gcra: &route,
            cost: 1,
        },
        Limit::Shared {
            gcra: &global,
            cost: 1,
        },
    ];
    let admitted = (0..20)
        .filter(|_| take_all(&limits, now).admitted())
        .count();
    assert_eq!(admitted, 5, "the tenant's burst");
    assert_eq!(global.remaining(now), 995);
    assert_eq!(route.remaining(now), 95);
    assert_eq!(per_client.remaining(b"10.0.0.1", now), Some(15));
    assert!(matches!(
        take_all(&limits, now),
        MultiDecision::Refused { index: 0, .. }
    ));
}

/// The upstream answers 200 with an `x-cost` header from the request's
/// own `x-charge`.
fn upstream()
-> impl FnMut(Request<()>) -> BoxFuture<'static, (Response<Body>, Delivery)> + Send + 'static {
    |req: Request<()>| {
        let charge = req.headers().get("x-charge").cloned();
        Box::pin(async move {
            let mut r = Response::new(full("ok"));
            if let Some(c) = charge {
                r.headers_mut().insert("x-cost", c);
            }
            (r, Delivery::Upstream)
        })
    }
}

#[tokio::test(start_paused = true)]
async fn costs_and_bursts_through_the_filter_phases() {
    let section: HttpLimitsSection = toml::from_str(
        r#"
        [[http_limits]]
        name = "upload"
        rate = "1000/s burst 10000"
        key = ["header:x-user"]
        cost = "request_bytes"
        cost_missing = 10000
        [http_limits.match]
        path_prefix = "/upload"

        [[http_limits]]
        name = "gql"
        rate = "100/s burst 300"
        key = ["header:x-user"]
        cost = "graphql"
        [http_limits.match]
        path = "/graphql"

        [[http_limits]]
        name = "billing"
        rate = "10/s burst 10"
        key = ["header:x-user"]
        cost = "response_header:x-cost"
        [http_limits.match]
        path_prefix = "/llm"

        [[http_limits]]
        name = "queue"
        rate = "5/s burst 1"
        key = ["header:x-user"]
        action = "delay"
        max_delay = "1s"
        [http_limits.match]
        path_prefix = "/slow"
        "#,
    )
    .unwrap();
    let clock = Arc::new(ManualClock::new(5 * SEC));
    let l =
        Arc::new(HttpLimiter::new(build(&section.http_limits).unwrap(), clock.clone()).unwrap());
    let f = l.route_filters(1).unwrap();
    let send = |path: &str, headers: &[(&str, &str)]| {
        let mut b = Request::post(path)
            .header("host", "api.test")
            .header("x-user", "u");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        run(f.get(0), 0, b.body(()).unwrap(), upstream())
    };
    // Bytes: 4 KiB bodies against 10,000 bytes of burst.
    assert_eq!(
        send("/upload/a", &[("content-length", "4096")])
            .await
            .status(),
        200
    );
    assert_eq!(
        send("/upload/b", &[("content-length", "4096")])
            .await
            .status(),
        200
    );
    let r = send("/upload/c", &[("content-length", "4096")]).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    // A body with no length costs cost_missing: the whole burst.
    clock.advance(10 * SEC);
    assert_eq!(
        send("/upload/d", &[("transfer-encoding", "chunked")])
            .await
            .status(),
        200
    );
    assert_eq!(
        send("/upload/e", &[("content-length", "1")]).await.status(),
        429
    );
    // GraphQL by complexity, from a GET query.
    let q = "%7B%20a(first%3A%20100)%20%7B%20b%20%7D%20%7D"; // { a(first: 100) { b } }
    let get = |q: &str| {
        let req = Request::get(format!("/graphql?query={q}"))
            .header("host", "api.test")
            .header("x-user", "u")
            .body(())
            .unwrap();
        run(f.get(0), 0, req, upstream())
    };
    for _ in 0..2 {
        let r = get(q).await;
        assert_eq!(r.status(), 200);
    }
    assert!(
        get(q).await.headers()["retry-after"].to_str().is_ok(),
        "202 + 101 passes 300"
    );
    // Charged after the response: the upstream says 25, which puts the
    // key 15 into debt; it waits until that is paid.
    assert_eq!(send("/llm", &[("x-charge", "25")]).await.status(), 200);
    let r = send("/llm", &[("x-charge", "1")]).await;
    assert_eq!(r.status(), 429);
    assert_eq!(
        r.headers()["retry-after"],
        "2",
        "1.6 s to pay 15 back and get 1"
    );
    clock.advance(1_600_000_000);
    assert_eq!(send("/llm", &[("x-charge", "1")]).await.status(), 200);
    // A burst held until its turns: with the clock standing still, each
    // request books the next token, 200 ms after the one before, up to
    // max_delay; the next is refused.
    for n in 0..=5u64 {
        let start = tokio::time::Instant::now();
        assert_eq!(send("/slow", &[]).await.status(), 200, "{n}");
        assert_eq!(start.elapsed(), Duration::from_millis(200 * n));
    }
    let r = send("/slow", &[]).await;
    assert_eq!(
        r.status(),
        429,
        "a seventh would wait 1.2 s, past max_delay"
    );
    let stats = l.stats();
    assert_eq!(stats[2].charged_after, 2);
    assert_eq!(stats[3].delayed, 5);
    // The NGINX zone the same queue copies: rate=5r/s burst=3.
    let zone = LimitReqTable::new(
        LimitReq {
            rate_milli: LimitReq::parse_rate("5r/s").unwrap(),
            burst: 3,
            delay: Delay::All,
        },
        1024,
    );
    let got: Vec<Outcome> = (0..5).map(|_| zone.decide(b"u", 1000)).collect();
    assert_eq!(
        got,
        [
            Outcome::Pass { delay_ms: 0 },
            Outcome::Pass { delay_ms: 200 },
            Outcome::Pass { delay_ms: 400 },
            Outcome::Pass { delay_ms: 600 },
            Outcome::Refuse
        ]
    );
}
