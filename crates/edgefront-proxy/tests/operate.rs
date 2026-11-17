//! Operating a running proxy, over real TCP: steer's metric series as the
//! request path, the checkers and reloads record them, the `/clusters`
//! state, reloads that keep endpoint state, and draining.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use edgefront_core::config::Config;
use edgefront_core::filter::{BoxFuture, Decision, Filter, RequestHead, RouteFilters};
use edgefront_core::listener::{ServerLimits, serve};
use edgefront_proxy::fakeupstream::{self, Server as Fake};
use edgefront_proxy::server::Proxy;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// A proxy listening on a free port with its checks running, stopped when
/// dropped.
struct Running {
    addr: SocketAddr,
    proxy: Arc<Proxy>,
    _stop: oneshot::Sender<()>,
    _stop_checks: oneshot::Sender<()>,
}

fn parse(config: &str) -> Config {
    Config::from_json(config.as_bytes()).expect("a valid config")
}

async fn start(config: &str) -> Running {
    run(Proxy::new(&parse(config))).await
}

async fn run(proxy: Proxy) -> Running {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let (stop_checks, checks_stopped) = oneshot::channel::<()>();
    let proxy = Arc::new(proxy);
    tokio::spawn(serve(
        listener,
        Arc::clone(&proxy),
        ServerLimits::default(),
        async move {
            let _ = stopped.await;
        },
    ));
    let checks = Arc::clone(&proxy);
    tokio::spawn(async move {
        checks
            .run_checks(async move {
                let _ = checks_stopped.await;
            })
            .await;
    });
    Running {
        addr,
        proxy,
        _stop: stop,
        _stop_checks: stop_checks,
    }
}

/// One cluster `web` over `endpoints` behind `host` and every path, with
/// `extra` cluster fields.
fn config_on(host: &str, endpoints: &[&str], extra: &str) -> String {
    let eps: Vec<String> = endpoints
        .iter()
        .map(|a| format!("{{\"address\":\"{a}\"}}"))
        .collect();
    format!(
        "{{\"listen\":\"127.0.0.1:0\",\"routes\":[{{\"host\":\"{host}\",\"path_prefix\":\"/\",\"cluster\":\"web\"}}],\
         \"clusters\":[{{\"name\":\"web\",\"policy\":\"round_robin\",\"endpoints\":[{}]{extra}}}]}}",
        eps.join(",")
    )
}

fn config(endpoints: &[&str], extra: &str) -> String {
    config_on("*", endpoints, extra)
}

/// A retry block with `fields` added to steer's defaults.
fn retry(fields: &str) -> String {
    format!(",\"retry\":{{\"backoff_base\":\"1ms\",\"backoff_max\":\"2ms\"{fields}}}")
}

const EJECT: &str = ",\"outlier_detection\":{\"consecutive_gateway_failures\":3,\"interval\":\"10s\",\
                     \"base_ejection_time\":\"30s\",\"max_ejection_percent\":50}";

/// An upstream that answers every request with `reply`, counting them.
struct Scripted {
    addr: String,
    count: Arc<AtomicUsize>,
}

async fn scripted(reply: &'static [u8]) -> Scripted {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let count = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&count);
    tokio::spawn(async move {
        loop {
            let Ok((conn, _)) = listener.accept().await else {
                return;
            };
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let (r, mut w) = conn.into_split();
                let mut r = BufReader::new(r);
                loop {
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    counter.fetch_add(1, Ordering::SeqCst);
                    if w.write_all(reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    Scripted { addr, count }
}

/// An address nothing listens on.
async fn dead() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().to_string()
}

/// Serves the fake upstream on a free port until it is dropped.
async fn fake(name: &str) -> (String, Arc<Fake>, oneshot::Sender<()>) {
    let server = Arc::new(Fake::with_seed(name, 7));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (stop, stopped) = oneshot::channel::<()>();
    let limits = ServerLimits {
        drain_timeout: Duration::from_millis(50),
        ..ServerLimits::default()
    };
    tokio::spawn(serve(listener, Arc::clone(&server), limits, async move {
        let _ = stopped.await;
    }));
    (addr, server, stop)
}

fn blackhole(srv: &Fake) {
    srv.set(fakeupstream::Settings {
        blackhole: true,
        ..srv.settings()
    });
}

const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
const UNAVAILABLE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\n\r\ndown";
const GET: &[u8] = b"GET / HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n";

/// Sends `request` on a new connection and reads the answer until the
/// proxy closes it.
async fn exchange(addr: SocketAddr, request: &[u8]) -> String {
    let mut conn = TcpStream::connect(addr).await.unwrap();
    conn.write_all(request).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

fn status(resp: &str) -> &str {
    resp.get(9..12).unwrap_or(resp)
}

/// The value of one series in the proxy's exposition, or `None` when the
/// series is not there.
fn series(proxy: &Proxy, key: &str) -> Option<f64> {
    let text = proxy.metrics().registry().render();
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.parse().ok())
}

fn count(proxy: &Proxy, key: &str) -> u64 {
    series(proxy, key).unwrap_or_else(|| panic!("no series {key}")) as u64
}

async fn wait_until(cond: impl Fn() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached in 5 s");
}

#[tokio::test]
async fn requests_attempts_and_retries_are_counted_in_steer_series() {
    let up = scripted(OK).await;
    let down = dead().await;
    let proxy = start(&config(&[&down, &up.addr], &retry(""))).await;
    for _ in 0..4 {
        assert_eq!(status(&exchange(proxy.addr, GET).await), "200");
    }
    let p = &proxy.proxy;
    assert_eq!(
        count(p, r#"steer_requests_total{cluster="web",code_class="2xx"}"#),
        4
    );
    assert_eq!(
        count(p, r#"steer_requests_total{cluster="web",code_class="5xx"}"#),
        0
    );
    assert_eq!(
        count(p, r#"steer_request_duration_seconds_count{cluster="web"}"#),
        4
    );
    let ok = format!(
        r#"steer_upstream_attempts_total{{cluster="web",endpoint="{}",result="success"}}"#,
        up.addr
    );
    assert_eq!(count(p, &ok), 4);
    // Round robin starts on either endpoint; each time it lands on the
    // dead one, the connect error is retried on the live one.
    let refused = format!(
        r#"steer_upstream_attempts_total{{cluster="web",endpoint="{down}",result="connect_error"}}"#
    );
    let refused = count(p, &refused);
    assert!((1..=4).contains(&refused), "{refused}");
    assert_eq!(count(p, r#"steer_retries_total{cluster="web"}"#), refused);
    assert_eq!(
        count(p, r#"steer_retry_budget_exhausted_total{cluster="web"}"#),
        0
    );
    assert_eq!(up.count.load(Ordering::SeqCst), 4);
    assert_eq!(count(p, "steer_config_version"), 1);
}

#[tokio::test]
async fn a_spent_budget_and_a_missing_route_are_counted() {
    let a = scripted(UNAVAILABLE).await;
    let b = scripted(UNAVAILABLE).await;
    let proxy = start(&config_on(
        "a",
        &[&a.addr, &b.addr],
        &retry(",\"budget\":{\"ratio\":0,\"min_per_second\":1,\"window\":\"1s\"}"),
    ))
    .await;
    for _ in 0..5 {
        assert_eq!(status(&exchange(proxy.addr, GET).await), "503");
    }
    let p = &proxy.proxy;
    // The floor allows one retry a second; every other 503 finds the
    // budget spent. The five requests may straddle a second.
    let retries = count(p, r#"steer_retries_total{cluster="web"}"#);
    let refused = count(p, r#"steer_retry_budget_exhausted_total{cluster="web"}"#);
    assert!((1..=2).contains(&retries), "{retries}");
    assert_eq!(retries + refused, 5);
    assert_eq!(
        count(p, r#"steer_requests_total{cluster="web",code_class="5xx"}"#),
        5
    );
    let resp = exchange(
        proxy.addr,
        b"GET / HTTP/1.1\r\nHost: elsewhere\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status(&resp), "404", "{resp}");
    assert_eq!(count(p, "steer_no_route_total"), 1);
    // A request with no route has no cluster to be counted under.
    assert_eq!(
        count(p, r#"steer_requests_total{cluster="web",code_class="4xx"}"#),
        0
    );
}

#[tokio::test]
async fn overflow_is_counted_and_in_flight_shows_in_the_state() {
    let (addr, srv, _s) = fake("slow").await;
    blackhole(&srv);
    let proxy = start(&config(&[&addr], ",\"max_requests\":1,\"timeout\":\"1s\"")).await;
    let held = tokio::spawn(exchange(proxy.addr, GET));
    let p = Arc::clone(&proxy.proxy);
    wait_until(|| p.state().clusters[0].in_flight == 1).await;
    let resp = exchange(proxy.addr, GET).await;
    assert_eq!(status(&resp), "503", "{resp}");
    assert!(
        resp.contains("x-steer-overflow: cluster_max_requests"),
        "{resp}"
    );
    assert_eq!(count(&p, r#"steer_overflow_total{cluster="web"}"#), 1);
    assert_eq!(status(&held.await.unwrap()), "504");
    wait_until(|| p.state().clusters[0].in_flight == 0).await;
    assert_eq!(
        count(
            &p,
            r#"steer_requests_total{cluster="web",code_class="5xx"}"#
        ),
        2
    );
    let timeouts = format!(
        r#"steer_upstream_attempts_total{{cluster="web",endpoint="{addr}",result="timeout"}}"#
    );
    // The cluster timeout cancels the attempt, which steer did not count.
    assert_eq!(count(&p, &timeouts), 0);
}

#[tokio::test]
async fn a_client_that_leaves_is_counted_as_499() {
    let (addr, srv, _s) = fake("slow").await;
    blackhole(&srv);
    let proxy = start(&config(&[&addr], ",\"timeout\":\"10s\"")).await;
    let p = Arc::clone(&proxy.proxy);
    let mut conn = TcpStream::connect(proxy.addr).await.unwrap();
    conn.write_all(GET).await.unwrap();
    wait_until(|| p.state().clusters[0].in_flight == 1).await;
    drop(conn);
    wait_until(|| {
        series(
            &p,
            r#"steer_requests_total{cluster="web",code_class="4xx"}"#,
        ) == Some(1.0)
    })
    .await;
    wait_until(|| p.state().clusters[0].in_flight == 0).await;
}

#[tokio::test]
async fn ejections_and_health_transitions_are_counted() {
    let bad = scripted(UNAVAILABLE).await;
    let good = scripted(OK).await;
    let proxy = start(&config(&[&bad.addr, &good.addr], EJECT)).await;
    for _ in 0..10 {
        exchange(proxy.addr, GET).await;
    }
    let p = &proxy.proxy;
    assert_eq!(
        count(
            p,
            r#"steer_outlier_ejections_total{cluster="web",reason="consecutive_gateway_failure"}"#
        ),
        1
    );
    let http_5xx = format!(
        r#"steer_upstream_attempts_total{{cluster="web",endpoint="{}",result="http_5xx"}}"#,
        bad.addr
    );
    assert_eq!(count(p, &http_5xx), 3);

    let (sick, sick_srv, _s) = fake("sick").await;
    blackhole(&sick_srv);
    let proxy = start(&config(
        &[&sick],
        ",\"health_check\":{\"path\":\"/healthz\",\"interval\":\"30ms\",\
         \"jitter\":\"5ms\",\"timeout\":\"20ms\",\"healthy_threshold\":1,\"unhealthy_threshold\":1}",
    ))
    .await;
    let p = Arc::clone(&proxy.proxy);
    let to = |v: &str| format!(r#"steer_health_transitions_total{{cluster="web",to="{v}"}}"#);
    wait_until(|| series(&p, &to("unhealthy")) == Some(1.0)).await;
    sick_srv.set(fakeupstream::Settings {
        blackhole: false,
        ..sick_srv.settings()
    });
    wait_until(|| series(&p, &to("healthy")) == Some(1.0)).await;
}

#[tokio::test]
async fn the_state_reports_clusters_as_steer_does() {
    let bad = scripted(UNAVAILABLE).await;
    let good = scripted(OK).await;
    let before = SystemTime::now();
    let proxy = start(&format!(
        "{{\"listen\":\"127.0.0.1:0\",\"routes\":[{{\"host\":\"*\",\"path_prefix\":\"/\",\"cluster\":\"web\"}}],\
         \"clusters\":[{{\"name\":\"web\",\"policy\":\"least_loaded\",\"max_requests\":9,\"endpoints\":[\
         {{\"address\":\"{}\",\"weight\":3}},{{\"address\":\"{}\",\"priority\":1}}]{EJECT}{}}},\
         {{\"name\":\"other\",\"endpoints\":[{{\"address\":\"{}\"}}]}}]}}",
        bad.addr,
        good.addr,
        retry(""),
        good.addr
    ))
    .await;
    for _ in 0..3 {
        exchange(proxy.addr, GET).await;
    }
    let state = proxy.proxy.state();
    assert_eq!(state.config_version, 1);
    assert!(state.loaded_at >= before && state.loaded_at <= SystemTime::now());
    assert_eq!(state.clusters.len(), 2);
    let web = &state.clusters[0];
    assert_eq!(web.name, "web");
    assert_eq!(web.policy, "least_loaded");
    assert_eq!(web.max_requests, 9);
    assert_eq!(web.in_flight, 0);
    assert!(web.retry_budget_balance.is_some());
    assert_eq!(web.balancer.total_per_priority, [1, 1]);
    assert_eq!(web.balancer.healthy_per_priority, [0, 1]);
    assert_eq!(web.balancer.priority_load_percent, [0, 100]);
    assert!(!web.balancer.panic);
    let (b, g) = (&web.endpoints[0], &web.endpoints[1]);
    assert_eq!(
        (b.address.as_str(), b.weight, b.priority),
        (&*bad.addr, 3, 0)
    );
    assert_eq!(
        (g.address.as_str(), g.weight, g.priority),
        (&*good.addr, 1, 1)
    );
    assert!(b.healthy && b.ejected && !b.available);
    assert_eq!(b.times_ejected, 1);
    assert_eq!(b.last_eject_reason, "consecutive_gateway_failure");
    // Ejected for the 30 s base, on the wall clock.
    let until = b.ejected_until.expect("ejected");
    let left = until
        .duration_since(SystemTime::now())
        .expect("in the future");
    assert!(
        left > Duration::from_secs(25) && left <= Duration::from_secs(30),
        "{left:?}"
    );
    assert!(g.available && g.ejected_until.is_none());
    assert!(
        b.ewma_latency_ms > g.ewma_latency_ms,
        "failures cost latency"
    );
    let other = &state.clusters[1];
    assert_eq!(
        (other.name.as_str(), other.policy.as_str()),
        ("other", "round_robin")
    );
    assert_eq!(other.retry_budget_balance, None);
    // The JSON steer served, with this crate's shape.
    let json = state.to_json().indented().unwrap();
    assert!(json.contains("\"policy\": \"least_loaded\""), "{json}");
}

#[tokio::test]
async fn a_reload_keeps_endpoint_state_by_address() {
    let bad = scripted(UNAVAILABLE).await;
    let good = scripted(OK).await;
    let proxy = start(&config(&[&bad.addr, &good.addr], EJECT)).await;
    for _ in 0..10 {
        exchange(proxy.addr, GET).await;
    }
    let p = &proxy.proxy;
    let old = p.endpoints("web").unwrap();
    assert!(old[0].state.ejected(std::time::Instant::now()));

    let extra = dead().await;
    let version = p
        .reload(&parse(&config(&[&extra, &bad.addr, &good.addr], EJECT)))
        .unwrap();
    assert_eq!(version, 2);
    assert_eq!(p.config_version(), 2);
    let new = p.endpoints("web").unwrap();
    assert_eq!(new.len(), 3);
    assert!(Arc::ptr_eq(&new[1].state, &old[0].state));
    assert!(Arc::ptr_eq(&new[2].state, &old[1].state));
    assert!(new[1].state.ejected(std::time::Instant::now()));
    assert_eq!(new[1].state.times_ejected(), 1);
    assert_eq!(new[0].state.times_ejected(), 0);
    // Traffic on the new configuration still avoids the ejected endpoint.
    let before = bad.count.load(Ordering::SeqCst);
    for _ in 0..6 {
        exchange(proxy.addr, GET).await;
    }
    assert_eq!(bad.count.load(Ordering::SeqCst), before);
    assert_eq!(
        count(p, r#"steer_config_reloads_total{result="success"}"#),
        1
    );
    assert_eq!(count(p, "steer_config_version"), 2);
    // The series of a kept endpoint carry on; the new one has its own.
    let ejections =
        r#"steer_outlier_ejections_total{cluster="web",reason="consecutive_gateway_failure"}"#;
    assert_eq!(count(p, ejections), 1);
    let extra_series = format!(
        r#"steer_upstream_attempts_total{{cluster="web",endpoint="{extra}",result="connect_error"}}"#
    );
    assert!(count(p, &extra_series) >= 1);
    assert_eq!(p.state().config_version, 2);
}

#[tokio::test]
async fn a_reload_without_the_checks_lets_endpoints_back() {
    let bad = scripted(UNAVAILABLE).await;
    let good = scripted(OK).await;
    let proxy = start(&config(&[&bad.addr, &good.addr], EJECT)).await;
    for _ in 0..10 {
        exchange(proxy.addr, GET).await;
    }
    let p = &proxy.proxy;
    let eps = p.endpoints("web").unwrap();
    eps[1].state.set_healthy(false);
    p.reload(&parse(&config(&[&bad.addr, &good.addr], "")))
        .unwrap();
    let now = std::time::Instant::now();
    // Nothing will end the ejection or probe the endpoint again.
    assert!(!eps[0].state.ejected(now));
    assert!(eps[1].state.healthy());
    let before = bad.count.load(Ordering::SeqCst);
    for _ in 0..4 {
        exchange(proxy.addr, GET).await;
    }
    assert_eq!(bad.count.load(Ordering::SeqCst), before + 2);
}

#[tokio::test]
async fn a_reload_keeps_the_in_flight_count_and_an_unchanged_budget() {
    let (slow, srv, _s) = fake("slow").await;
    blackhole(&srv);
    let a = scripted(UNAVAILABLE).await;
    let b = scripted(UNAVAILABLE).await;
    let spend = retry(",\"budget\":{\"ratio\":0,\"min_per_second\":1,\"window\":\"10s\"}");
    let both = format!(
        "{{\"listen\":\"127.0.0.1:0\",\"routes\":[{{\"host\":\"slow\",\"path_prefix\":\"/\",\"cluster\":\"slow\"}},\
         {{\"host\":\"*\",\"path_prefix\":\"/\",\"cluster\":\"web\"}}],\"clusters\":[\
         {{\"name\":\"slow\",\"max_requests\":MAX,\"timeout\":\"2s\",\"endpoints\":[{{\"address\":\"{slow}\"}}]}},\
         {{\"name\":\"web\",\"endpoints\":[{{\"address\":\"{}\"}},{{\"address\":\"{}\"}}]BUDGET}}]}}",
        a.addr, b.addr
    );
    let cfg = |max: u32, budget: &str| {
        both.replace("MAX", &max.to_string())
            .replace("BUDGET", budget)
    };
    let proxy = start(&cfg(1, &spend)).await;
    let p = Arc::clone(&proxy.proxy);
    // Spend one of the window's ten retries.
    exchange(proxy.addr, GET).await;
    assert!(p.retry_budget_balance("web").unwrap() < 10.0);
    let held = tokio::spawn(exchange(
        proxy.addr,
        b"GET / HTTP/1.1\r\nHost: slow\r\nConnection: close\r\n\r\n",
    ));
    wait_until(|| p.state().clusters[0].in_flight == 1).await;

    p.reload(&parse(&cfg(1, &spend))).unwrap();
    assert_eq!(p.state().clusters[0].in_flight, 1, "the count carries over");
    assert!(
        p.retry_budget_balance("web").unwrap() < 10.0,
        "the same budget"
    );
    let resp = exchange(
        proxy.addr,
        b"GET / HTTP/1.1\r\nHost: slow\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status(&resp), "503", "still at max_requests: {resp}");

    // A higher limit applies at once to the same count; a changed budget
    // starts afresh.
    p.reload(&parse(&cfg(
        2,
        &retry(",\"budget\":{\"ratio\":0,\"min_per_second\":1,\"window\":\"5s\"}"),
    )))
    .unwrap();
    assert!((p.retry_budget_balance("web").unwrap() - 5.0).abs() < 1e-9);
    assert_eq!(p.state().clusters[0].max_requests, 2);
    let second = tokio::spawn(exchange(
        proxy.addr,
        b"GET / HTTP/1.1\r\nHost: slow\r\nConnection: close\r\n\r\n",
    ));
    wait_until(|| p.state().clusters[0].in_flight == 2).await;
    assert_eq!(status(&held.await.unwrap()), "504");
    assert_eq!(status(&second.await.unwrap()), "504");
    wait_until(|| p.state().clusters[0].in_flight == 0).await;
    assert_eq!(p.config_version(), 3);
}

#[tokio::test]
async fn health_checks_follow_a_reload() {
    let (sick, srv, _s) = fake("sick").await;
    blackhole(&srv);
    let checked = ",\"health_check\":{\"path\":\"/healthz\",\"interval\":\"30ms\",\
                   \"jitter\":\"5ms\",\"timeout\":\"20ms\",\"healthy_threshold\":1,\"unhealthy_threshold\":1}";
    let proxy = start(&config(&[&sick], "")).await;
    let p = Arc::clone(&proxy.proxy);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let eps = p.endpoints("web").unwrap();
    assert!(eps[0].state.healthy(), "nothing probes it yet");
    p.reload(&parse(&config(&[&sick], checked))).unwrap();
    wait_until(|| !eps[0].state.healthy()).await;
    p.reload(&parse(&config(&[&sick], ""))).unwrap();
    assert!(eps[0].state.healthy(), "nothing will probe it back up");
    // The checker of the second configuration has stopped.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(eps[0].state.healthy());
}

struct Passthrough;

impl Filter for Passthrough {
    fn request<'a>(&'a self, _req: RequestHead<'a>) -> BoxFuture<'a, Decision> {
        Box::pin(async { Decision::Forward(None) })
    }
}

#[tokio::test]
async fn a_reload_that_cannot_apply_is_refused_and_counted() {
    let up = scripted(OK).await;
    let text = config(&[&up.addr], "");
    let proxy = start(&text).await;
    let p = &proxy.proxy;
    let mut moved = parse(&text);
    moved.listen = "127.0.0.1:1".into();
    assert_eq!(
        p.reload(&moved).unwrap_err(),
        "listen and admin_listen cannot change on reload; restart edgefront instead"
    );
    let mut moved = parse(&text);
    moved.admin_listen = "127.0.0.1:2".into();
    assert!(p.reload(&moved).is_err());
    p.record_failed_reload();
    assert_eq!(
        count(p, r#"steer_config_reloads_total{result="failure"}"#),
        3
    );
    assert_eq!(
        count(p, r#"steer_config_reloads_total{result="success"}"#),
        0
    );
    assert_eq!(p.config_version(), 1);
    assert_eq!(status(&exchange(proxy.addr, GET).await), "200");

    let mut filters = RouteFilters::new(1);
    filters.set(0, Arc::new(Passthrough));
    let filtered = Proxy::new(&parse(&text)).with_filters(filters);
    assert_eq!(
        filtered.reload(&parse(&text)).unwrap_err(),
        "routes with filters cannot be reloaded; restart edgefront instead"
    );
}

#[tokio::test]
async fn draining_closes_each_connection_after_its_response() {
    let up = scripted(OK).await;
    let proxy = start(&config(&[&up.addr], "")).await;
    let keep = b"GET / HTTP/1.1\r\nHost: a\r\n\r\n";
    let mut conn = TcpStream::connect(proxy.addr).await.unwrap();
    conn.write_all(keep).await.unwrap();
    let mut buf = vec![0; 4096];
    let n = conn.read(&mut buf).await.unwrap();
    let first = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert_eq!(status(&first), "200");
    assert!(
        !first.to_ascii_lowercase().contains("connection: close"),
        "{first}"
    );
    assert!(!proxy.proxy.draining());
    proxy.proxy.drain();
    assert!(proxy.proxy.draining());
    conn.write_all(keep).await.unwrap();
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), conn.read_to_end(&mut rest))
        .await
        .expect("the proxy closes the connection")
        .unwrap();
    let second = String::from_utf8_lossy(&rest).into_owned();
    assert_eq!(status(&second), "200");
    assert!(
        second.to_ascii_lowercase().contains("connection: close"),
        "{second}"
    );
}
