//! Health checks, outlier detection and retries on the proxy's request
//! path, over real TCP: steer's attempt loop (`transport.go`) and the
//! checkers its proxy started beside the listener. Upstreams are scripted
//! with hand-written HTTP/1.1 or are the fake upstream ported from steer.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use edgefront_core::config::Config;
use edgefront_core::listener::{ServerLimits, serve};
use edgefront_proxy::fakeupstream::{self, Server as Fake};
use edgefront_proxy::server::Proxy;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

/// A proxy listening on a free port with its checks running, stopped when
/// dropped.
struct Running {
    addr: SocketAddr,
    proxy: Arc<Proxy>,
    _stop: oneshot::Sender<()>,
    _stop_checks: oneshot::Sender<()>,
}

async fn start(config: &str) -> Running {
    let cfg = Config::from_json(config.as_bytes()).expect("a valid config");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let (stop_checks, checks_stopped) = oneshot::channel::<()>();
    let proxy = Arc::new(Proxy::new(&cfg));
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

/// One cluster `web` over `endpoints` behind every host and path, with
/// `extra` cluster fields.
fn config(endpoints: &[&str], extra: &str) -> String {
    let eps: Vec<String> = endpoints
        .iter()
        .map(|a| format!("{{\"address\":\"{a}\"}}"))
        .collect();
    format!(
        "{{\"listen\":\"127.0.0.1:0\",\"routes\":[{{\"host\":\"*\",\"path_prefix\":\"/\",\"cluster\":\"web\"}}],\
         \"clusters\":[{{\"name\":\"web\",\"policy\":\"round_robin\",\"endpoints\":[{}]{extra}}}]}}",
        eps.join(",")
    )
}

/// A retry block with `fields` added to steer's defaults.
fn retry(fields: &str) -> String {
    format!(",\"retry\":{{\"backoff_base\":\"1ms\",\"backoff_max\":\"2ms\"{fields}}}")
}

/// An upstream that answers every request with `reply` and reports each
/// request (head and body) it read.
struct Scripted {
    addr: String,
    seen: mpsc::UnboundedReceiver<String>,
    count: Arc<AtomicUsize>,
}

async fn scripted(reply: &'static [u8]) -> Scripted {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, seen) = mpsc::unbounded_channel();
    let count = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&count);
    tokio::spawn(async move {
        loop {
            let Ok((conn, _)) = listener.accept().await else {
                return;
            };
            let tx = tx.clone();
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let (r, mut w) = conn.into_split();
                let mut r = BufReader::new(r);
                loop {
                    let mut head = String::new();
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = v.trim().parse().unwrap();
                        }
                        head.push_str(&line);
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let mut body = vec![0; length];
                    if r.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    head.push_str(&String::from_utf8_lossy(&body));
                    counter.fetch_add(1, Ordering::SeqCst);
                    let _ = tx.send(head);
                    if w.write_all(reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    Scripted { addr, seen, count }
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

const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
const UNAVAILABLE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\n\r\ndown";
const INTERNAL: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\n\r\noops";

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

const GET: &[u8] = b"GET / HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n";

#[tokio::test]
async fn a_connect_error_is_retried_on_another_endpoint() {
    let up = scripted(OK).await;
    let down = dead().await;
    let proxy = start(&config(&[&down, &up.addr], &retry(""))).await;
    // POST is not idempotent, but a connect error never reached anyone.
    for req in [
        GET,
        b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc"
            .as_slice(),
    ] {
        for _ in 0..4 {
            let resp = exchange(proxy.addr, req).await;
            assert_eq!(status(&resp), "200", "{resp}");
        }
    }
    assert_eq!(up.count.load(Ordering::SeqCst), 8);
}

#[tokio::test]
async fn without_retries_a_connect_error_is_502() {
    let up = scripted(OK).await;
    let down = dead().await;
    let proxy = start(&config(&[&down, &up.addr], "")).await;
    let mut codes = Vec::new();
    for _ in 0..4 {
        let resp = exchange(proxy.addr, GET).await;
        codes.push(status(&resp).to_owned());
        if status(&resp) == "502" {
            assert!(resp.contains(r#""code":"bad_gateway""#), "{resp}");
        }
    }
    codes.sort();
    assert_eq!(codes, ["200", "200", "502", "502"]);
}

#[tokio::test]
async fn a_5xx_in_retry_on_is_retried_only_when_safe() {
    let bad = scripted(UNAVAILABLE).await;
    let mut good = scripted(OK).await;
    let proxy = start(&config(&[&bad.addr, &good.addr], &retry(""))).await;
    // Round robin starts at the first endpoint each pair of requests.
    let resp = exchange(proxy.addr, GET).await;
    assert_eq!(status(&resp), "200", "{resp}");
    assert_eq!(bad.count.load(Ordering::SeqCst), 1);

    // A POST without an Idempotency-Key gets the 503.
    let post = b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";
    let mut answers = Vec::new();
    for _ in 0..2 {
        answers.push(status(&exchange(proxy.addr, post).await).to_owned());
    }
    answers.sort();
    assert_eq!(answers, ["200", "503"]);

    // With one, it is retried, and the body is replayed whole.
    while good.seen.try_recv().is_ok() {}
    let keyed = b"POST / HTTP/1.1\r\nHost: a\r\nIdempotency-Key: k1\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";
    for _ in 0..2 {
        let resp = exchange(proxy.addr, keyed).await;
        assert_eq!(status(&resp), "200", "{resp}");
    }
    for _ in 0..2 {
        let seen = good.seen.recv().await.unwrap();
        assert!(seen.ends_with("\r\n\r\nhello"), "{seen}");
        assert!(
            seen.contains("Content-Length: 5\r\n") || seen.contains("content-length: 5\r\n"),
            "{seen}"
        );
    }
}

#[tokio::test]
async fn a_5xx_outside_retry_on_is_delivered() {
    let bad = scripted(INTERNAL).await;
    let good = scripted(OK).await;
    let proxy = start(&config(&[&bad.addr, &good.addr], &retry(""))).await;
    let mut answers = Vec::new();
    for _ in 0..2 {
        let resp = exchange(proxy.addr, GET).await;
        answers.push(status(&resp).to_owned());
        if status(&resp) == "500" {
            assert!(resp.ends_with("oops"), "{resp}");
        }
    }
    answers.sort();
    assert_eq!(answers, ["200", "500"]);
    // With 500 listed it is retried.
    let proxy = start(&config(
        &[&bad.addr, &good.addr],
        &retry(",\"retry_on\":[500]"),
    ))
    .await;
    for _ in 0..4 {
        assert_eq!(status(&exchange(proxy.addr, GET).await), "200");
    }
}

#[tokio::test]
async fn a_body_over_max_body_bytes_streams_and_is_not_retried() {
    let bad = scripted(UNAVAILABLE).await;
    let mut good = scripted(OK).await;
    let proxy = start(&config(
        &[&bad.addr, &good.addr],
        &retry(",\"max_body_bytes\":4"),
    ))
    .await;
    let req = b"PUT / HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";
    let mut answers = Vec::new();
    for _ in 0..2 {
        answers.push(status(&exchange(proxy.addr, req).await).to_owned());
    }
    answers.sort();
    assert_eq!(answers, ["200", "503"]);
    assert_eq!(bad.count.load(Ordering::SeqCst), 1);
    assert!(good.seen.recv().await.unwrap().ends_with("hello"));
}

#[tokio::test]
async fn every_endpoint_failing_ends_with_the_last_answer() {
    let a = scripted(UNAVAILABLE).await;
    let b = scripted(UNAVAILABLE).await;
    let c = scripted(UNAVAILABLE).await;
    let proxy = start(&config(
        &[&a.addr, &b.addr, &c.addr],
        &retry(",\"max_retries\":5"),
    ))
    .await;
    let resp = exchange(proxy.addr, GET).await;
    assert_eq!(status(&resp), "503", "{resp}");
    assert!(resp.ends_with("down"), "{resp}");
    // Each endpoint once: a retry never goes back to one already tried.
    for up in [&a, &b, &c] {
        assert_eq!(up.count.load(Ordering::SeqCst), 1);
    }

    // With every endpoint unreachable the last connect error is a 502.
    let (d1, d2) = (dead().await, dead().await);
    let proxy = start(&config(&[&d1, &d2], &retry(""))).await;
    let resp = exchange(proxy.addr, GET).await;
    assert_eq!(status(&resp), "502", "{resp}");
    assert!(resp.contains(r#""code":"bad_gateway""#), "{resp}");
}

#[tokio::test]
async fn retries_stop_when_the_budget_is_spent() {
    let a = scripted(UNAVAILABLE).await;
    let b = scripted(UNAVAILABLE).await;
    // No ratio and a floor of one retry a second over one second.
    let proxy = start(&config(
        &[&a.addr, &b.addr],
        &retry(",\"budget\":{\"ratio\":0,\"min_per_second\":1,\"window\":\"1s\"}"),
    ))
    .await;
    let budget = proxy.proxy.retry_budget_balance("web").unwrap();
    assert!((budget - 1.0).abs() < 1e-9, "{budget}");
    for _ in 0..6 {
        assert_eq!(status(&exchange(proxy.addr, GET).await), "503");
    }
    let sent = a.count.load(Ordering::SeqCst) + b.count.load(Ordering::SeqCst);
    // Six first attempts, and one or two retries: the second may fall in
    // the next second's floor, but never more than that in the time six
    // requests take.
    assert!((7..=8).contains(&sent), "{sent} attempts");
    assert!(proxy.proxy.retry_budget_balance("web").unwrap() <= 1.0);
    assert_eq!(proxy.proxy.retry_budget_balance("nope"), None);
}

#[tokio::test]
async fn the_per_try_timeout_moves_on_from_a_blackhole() {
    let (hole, hole_srv, _h) = fake("hole").await;
    let (live, live_srv, _l) = fake("live").await;
    hole_srv.set(fakeupstream::Settings {
        blackhole: true,
        ..hole_srv.settings()
    });
    let proxy = start(&config(
        &[&hole, &live],
        &(retry(",\"per_try_timeout\":\"100ms\"") + ",\"timeout\":\"2s\""),
    ))
    .await;
    for _ in 0..4 {
        let resp = exchange(proxy.addr, GET).await;
        assert_eq!(status(&resp), "200", "{resp}");
        assert!(resp.contains("live"), "{resp}");
    }
    assert_eq!(live_srv.counts().0, 4);

    // When every endpoint times out the answer is steer's 504.
    live_srv.set(fakeupstream::Settings {
        blackhole: true,
        ..live_srv.settings()
    });
    let started = std::time::Instant::now();
    let resp = exchange(proxy.addr, GET).await;
    assert_eq!(status(&resp), "504", "{resp}");
    assert!(resp.contains(r#""code":"upstream_timeout""#), "{resp}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "per try, not the cluster timeout"
    );
}

#[tokio::test]
async fn consecutive_gateway_failures_eject_and_traffic_moves_away() {
    let bad = scripted(UNAVAILABLE).await;
    let good = scripted(OK).await;
    let proxy = start(&config(
        &[&bad.addr, &good.addr],
        ",\"outlier_detection\":{\"consecutive_gateway_failures\":3,\"interval\":\"10s\",\
         \"base_ejection_time\":\"30s\",\"max_ejection_percent\":50}",
    ))
    .await;
    let mut unavailable = 0;
    for _ in 0..20 {
        if status(&exchange(proxy.addr, GET).await) == "503" {
            unavailable += 1;
        }
    }
    assert_eq!(unavailable, 3, "ejected after its third 503");
    let eps = proxy.proxy.endpoints("web").unwrap();
    let now = std::time::Instant::now();
    assert!(eps[0].state.ejected(now));
    assert!(!eps[1].state.ejected(now));
    assert_eq!(eps[0].state.times_ejected(), 1);
}

#[tokio::test]
async fn retries_and_ejection_together_hide_a_failing_endpoint() {
    let bad = scripted(UNAVAILABLE).await;
    let good = scripted(OK).await;
    let proxy = start(&config(
        &[&bad.addr, &good.addr],
        &(retry("")
            + ",\"outlier_detection\":{\"consecutive_gateway_failures\":2,\"interval\":\"10s\",\
               \"base_ejection_time\":\"30s\",\"max_ejection_percent\":50}"),
    ))
    .await;
    for _ in 0..10 {
        assert_eq!(status(&exchange(proxy.addr, GET).await), "200");
    }
    assert_eq!(bad.count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn health_checks_take_an_endpoint_out_and_put_it_back() {
    let (sick, sick_srv, _s) = fake("sick").await;
    let (well, _well_srv, _w) = fake("well").await;
    sick_srv.set(fakeupstream::Settings {
        blackhole: true,
        ..sick_srv.settings()
    });
    let proxy = start(&config(
        &[&sick, &well],
        ",\"timeout\":\"300ms\",\"health_check\":{\"path\":\"/healthz\",\"interval\":\"50ms\",\
         \"jitter\":\"10ms\",\"timeout\":\"40ms\",\"healthy_threshold\":1,\"unhealthy_threshold\":1}",
    ))
    .await;
    let eps = proxy.proxy.endpoints("web").unwrap();
    wait_until(|| !eps[0].state.healthy()).await;
    assert!(eps[1].state.healthy());
    for _ in 0..4 {
        let resp = exchange(proxy.addr, GET).await;
        assert_eq!(status(&resp), "200", "{resp}");
        assert!(resp.contains("well"), "{resp}");
    }
    sick_srv.set(fakeupstream::Settings {
        blackhole: false,
        ..sick_srv.settings()
    });
    wait_until(|| eps[0].state.healthy()).await;
}

#[tokio::test]
async fn run_checks_returns_on_shutdown() {
    let (addr, _srv, _s) = fake("a").await;
    let cfg = Config::from_json(
        config(
            &[&addr],
            ",\"health_check\":{\"path\":\"/healthz\",\"interval\":\"20ms\",\"timeout\":\"10ms\"},\
             \"outlier_detection\":{\"interval\":\"20ms\"}",
        )
        .as_bytes(),
    )
    .unwrap();
    let proxy = Proxy::new(&cfg);
    let (stop, stopped) = oneshot::channel::<()>();
    let run = proxy.run_checks(async move {
        let _ = stopped.await;
    });
    let stopper = async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(stop);
    };
    tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(run, stopper) })
        .await
        .expect("run_checks stops");
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
