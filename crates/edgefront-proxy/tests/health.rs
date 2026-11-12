//! Active health checks over real TCP, steer's `internal/health` tests
//! translated: thresholds, probe failures, the Host header, the schedule on
//! a manual clock and shutdown. The upstreams are the fake upstream ported
//! from steer, and a scripted one where a test needs a given status.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use edgefront_core::clock::{Clock, ManualClock, MonotonicClock};
use edgefront_core::config::HealthCheck;
use edgefront_core::listener::{Handler, ServerLimits, serve};
use edgefront_proxy::endpoint::Endpoint;
use edgefront_proxy::fakeupstream::{Server, Settings};
use edgefront_proxy::health::{ChangeFn, Checker};
use http::{Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

fn cfg() -> HealthCheck {
    HealthCheck {
        path: "/healthz".to_owned(),
        host: String::new(),
        interval: Duration::from_secs(1),
        jitter: Duration::from_millis(100),
        timeout: Duration::from_millis(200),
        healthy_threshold: 2,
        unhealthy_threshold: 3,
    }
}

/// Serves `handler` on a free port until the returned sender is dropped.
async fn start<H: Handler>(handler: Arc<H>) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let limits = ServerLimits {
        drain_timeout: Duration::from_millis(100),
        ..ServerLimits::default()
    };
    tokio::spawn(serve(listener, handler, limits, async move {
        let _ = stopped.await;
    }));
    (addr, stop)
}

/// Answers `/healthz` from steer's checker with `status` and `body_len`
/// bytes of body, anything else with 404, and remembers what it saw.
#[derive(Default)]
struct Scripted {
    status: AtomicU16,
    body_len: AtomicUsize,
    probes: AtomicUsize,
    host: Mutex<String>,
    location: Option<&'static str>,
}

impl Scripted {
    fn new(status: u16) -> Arc<Scripted> {
        let s = Scripted::default();
        s.status.store(status, Ordering::Relaxed);
        Arc::new(s)
    }

    fn host(&self) -> String {
        self.host.lock().unwrap().clone()
    }
}

impl Handler for Scripted {
    type Body = Full<Bytes>;

    async fn call(self: Arc<Self>, req: Request<Incoming>, _: SocketAddr) -> Response<Self::Body> {
        self.probes.fetch_add(1, Ordering::Relaxed);
        if let Some(h) = req.headers().get("host") {
            *self.host.lock().unwrap() = h.to_str().unwrap().to_owned();
        }
        let ua = req.headers().get("user-agent").map(|v| v.as_bytes());
        let close = req.headers().get("connection").map(|v| v.as_bytes());
        let mut resp = Response::new(Full::new(Bytes::from(vec![
            b'x';
            self.body_len
                .load(Ordering::Relaxed)
        ])));
        if req.uri().path() != "/healthz"
            || ua != Some(b"steer-health-check")
            || close != Some(b"close")
        {
            *resp.status_mut() = StatusCode::NOT_FOUND;
            return resp;
        }
        *resp.status_mut() = StatusCode::from_u16(self.status.load(Ordering::Relaxed)).unwrap();
        if let Some(l) = self.location {
            resp.headers_mut().insert("location", l.parse().unwrap());
        }
        resp
    }
}

/// Records verdict changes as `up:` or `down:` and the reason.
#[derive(Clone, Default)]
struct Changes(Arc<Mutex<Vec<String>>>);

impl Changes {
    fn callback(&self) -> Option<ChangeFn> {
        let got = Arc::clone(&self.0);
        Some(Box::new(
            move |_: &Endpoint, healthy: bool, reason: &str| {
                let s = if healthy { "up" } else { "down" };
                got.lock().unwrap().push(format!("{s}:{reason}"));
            },
        ))
    }

    fn list(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

fn endpoint(addr: &str) -> Endpoint {
    Endpoint::new(addr, 1, 0, Instant::now())
}

#[tokio::test]
async fn thresholds() {
    let up = Scripted::new(200);
    let (addr, _stop) = start(Arc::clone(&up)).await;
    let ep = endpoint(&addr.to_string());
    let ch = Changes::default();
    let c = Checker::new(
        cfg(),
        vec![ep.clone()],
        MonotonicClock::shared(),
        ch.callback(),
    );

    assert_eq!(c.check_once(&ep).await, (true, false));
    assert!(ch.list().is_empty(), "a healthy endpoint changed state");
    up.status.store(503, Ordering::Relaxed);
    assert_eq!(c.check_once(&ep).await, (true, false));
    assert_eq!(
        c.check_once(&ep).await,
        (true, false),
        "down before the threshold"
    );
    assert_eq!(
        c.check_once(&ep).await,
        (false, true),
        "up after 3 failures"
    );
    assert_eq!(ch.list(), ["down:probe returned status 503"]);
    let snap = ep.state.snapshot(Instant::now());
    assert_eq!(snap.last_probe_error, "probe returned status 503");
    assert!(!snap.available);

    up.status.store(204, Ordering::Relaxed);
    assert_eq!(c.check_once(&ep).await, (false, false), "up after one pass");
    assert_eq!(c.check_once(&ep).await, (true, true), "down after 2 passes");
    assert_eq!(ch.list().last().unwrap(), "up:");
    assert_eq!(up.host(), addr.to_string(), "Host is the endpoint address");
    assert_eq!(up.probes.load(Ordering::Relaxed), 6);
}

/// The fake upstream's health endpoint stays green through its error rate,
/// the case only passive outlier detection can catch, and goes dark with
/// the blackhole or with latency past the timeout.
#[tokio::test]
async fn against_the_fake_upstream() {
    let fake = Arc::new(Server::with_seed("a", 1));
    let (addr, _stop) = start(Arc::clone(&fake)).await;
    let ep = endpoint(&addr.to_string());
    let ch = Changes::default();
    let c = Checker::new(
        cfg(),
        vec![ep.clone()],
        MonotonicClock::shared(),
        ch.callback(),
    );

    fake.set(Settings {
        error_rate: 1.0,
        ..Settings::default()
    });
    for _ in 0..5 {
        assert_eq!(c.check_once(&ep).await, (true, false));
    }

    fake.set(Settings {
        blackhole: true,
        ..Settings::default()
    });
    let started = Instant::now();
    for _ in 0..3 {
        c.check_once(&ep).await;
    }
    assert!(
        started.elapsed() >= Duration::from_millis(600),
        "{:?}",
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(!ep.state.healthy());
    assert_eq!(ch.list(), ["down:probe failed: timed out"]);
    assert_eq!(fake.counts().2, 3, "every probe was held");

    fake.set(Settings {
        latency: Duration::from_millis(400),
        ..Settings::default()
    });
    assert_eq!(
        c.check_once(&ep).await,
        (false, false),
        "slower than the timeout"
    );
    fake.set(Settings {
        latency: Duration::from_millis(20),
        ..Settings::default()
    });
    assert_eq!(c.check_once(&ep).await, (false, false));
    assert_eq!(c.check_once(&ep).await, (true, true));
    assert_eq!(ch.list().len(), 2);
    fake.close();
}

#[tokio::test]
async fn probe_failures() {
    // Redirects are failures, not followed.
    let redirect = Arc::new(Scripted {
        location: Some("/elsewhere"),
        ..Scripted::default()
    });
    redirect.status.store(302, Ordering::Relaxed);
    let (redirect_addr, _s1) = start(Arc::clone(&redirect)).await;
    // A server that never answers: the timeout must fire.
    let slow = Arc::new(Server::new("slow"));
    slow.set(Settings {
        blackhole: true,
        ..Settings::default()
    });
    let (slow_addr, _s2) = start(Arc::clone(&slow)).await;
    // A closed port refuses connections.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_addr = closed.local_addr().unwrap();
    drop(closed);
    // A response head over 16 KiB.
    let big_head = raw_upstream(|| {
        let mut r = b"HTTP/1.1 200 OK\r\nX-Big: ".to_vec();
        r.extend(std::iter::repeat_n(b'a', 20 << 10));
        r.extend_from_slice(b"\r\nContent-Length: 0\r\n\r\n");
        r
    })
    .await;
    // Not HTTP at all.
    let garbage = raw_upstream(|| b"SSH-2.0-OpenSSH_9.6\r\n".to_vec()).await;

    let c = Checker::new(cfg(), Vec::new(), MonotonicClock::shared(), None);
    let cases = [
        (redirect_addr, "probe returned status 302"),
        (slow_addr, "probe failed: timed out"),
        (closed_addr, "probe failed: connection failed"),
        (big_head, "probe failed: response headers too large"),
        (garbage, "probe failed: malformed response"),
    ];
    for (addr, want) in cases {
        let started = Instant::now();
        let err = c.probe(&endpoint(&addr.to_string())).await.unwrap_err();
        assert_eq!(err, want, "{addr}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{addr}: {:?}",
            started.elapsed()
        );
    }
    assert_eq!(
        redirect.probes.load(Ordering::Relaxed),
        1,
        "the redirect was followed"
    );
    let bad = c.probe(&endpoint("bad host:1")).await.unwrap_err();
    assert!(bad.starts_with("invalid probe URL"), "{bad}");
    slow.close();
}

/// An upstream that answers every connection with `reply()` and closes.
async fn raw_upstream(reply: fn() -> Vec<u8>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let _ = conn.read(&mut buf).await;
                let _ = conn.write_all(&reply()).await;
            });
        }
    });
    addr
}

/// A pass needs only the first 4 KiB of a large body, and a body cut off
/// after a 2xx head is still a pass.
#[tokio::test]
async fn bodies_are_read_only_so_far() {
    let up = Scripted::new(200);
    up.body_len.store(8 << 20, Ordering::Relaxed);
    let (addr, _stop) = start(Arc::clone(&up)).await;
    let c = Checker::new(cfg(), Vec::new(), MonotonicClock::shared(), None);
    assert_eq!(c.probe(&endpoint(&addr.to_string())).await, Ok(()));

    let cut =
        raw_upstream(|| b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\nshort".to_vec()).await;
    assert_eq!(c.probe(&endpoint(&cut.to_string())).await, Ok(()));
}

#[tokio::test]
async fn custom_host_header() {
    let up = Scripted::new(200);
    let (addr, _stop) = start(Arc::clone(&up)).await;
    let mut hc = cfg();
    hc.host = "health.internal".to_owned();
    let c = Checker::new(hc.clone(), Vec::new(), MonotonicClock::shared(), None);
    assert_eq!(c.probe(&endpoint(&addr.to_string())).await, Ok(()));
    assert_eq!(up.host(), "health.internal");

    hc.host = "bad\nhost".to_owned();
    let c = Checker::new(hc, Vec::new(), MonotonicClock::shared(), None);
    let err = c.probe(&endpoint(&addr.to_string())).await.unwrap_err();
    assert!(err.starts_with("invalid probe Host header"), "{err}");
}

#[tokio::test]
async fn jitter_is_a_fraction_of_the_configured_jitter() {
    let c = Checker::new(cfg(), Vec::new(), MonotonicClock::shared(), None);
    for _ in 0..1000 {
        let w = c.next_wait();
        assert!(
            w >= cfg().interval && w < cfg().interval + cfg().jitter,
            "{w:?}"
        );
    }
    let c = c.with_random(|| 0.25);
    assert_eq!(c.next_wait(), Duration::from_millis(1025));
    assert!(format!("{c:?}").contains("Checker"));
}

/// Run probes on the injected clock: nothing happens between ticks, and
/// every tick probes again.
#[tokio::test]
async fn run_follows_the_clock() {
    let up = Scripted::new(503);
    let (addr, _stop) = start(Arc::clone(&up)).await;
    let clock = Arc::new(ManualClock::new());
    let eps = vec![endpoint(&addr.to_string())];
    let ch = Changes::default();
    let c = Arc::new(
        Checker::new(
            cfg(),
            eps.clone(),
            Arc::clone(&clock) as Arc<dyn Clock>,
            ch.callback(),
        )
        .with_random(|| 0.5),
    );
    assert_eq!(c.endpoints().len(), 1);
    let (shutdown, stopped) = oneshot::channel::<()>();
    let running = tokio::spawn(Arc::clone(&c).run(async move {
        let _ = stopped.await;
    }));
    let wait = cfg().interval + cfg().jitter / 2;
    for want in 1..=3 {
        assert!(
            clock.wait_for_waiters(1, Duration::from_secs(5)).await,
            "the probe loop is not waiting on the clock"
        );
        assert_eq!(up.probes.load(Ordering::Relaxed), want);
        // Just short of the wait, the loop sleeps on.
        clock.advance(wait - Duration::from_nanos(1));
        assert_eq!(clock.waiters(), 1);
        clock.advance(Duration::from_nanos(1));
    }
    assert!(
        clock.wait_for_waiters(1, Duration::from_secs(5)).await,
        "the probe loop stopped"
    );
    assert!(
        !eps[0].state.healthy(),
        "still healthy after 3 failed probes"
    );
    let _ = shutdown.send(());
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("run returns after shutdown")
        .unwrap();
    assert_eq!(ch.list(), ["down:probe returned status 503"]);
}

/// A probe cut short by shutdown must not count as a failure.
#[tokio::test]
async fn a_probe_cut_short_by_shutdown_is_not_recorded() {
    let fake = Arc::new(Server::new("hang"));
    fake.set(Settings {
        blackhole: true,
        ..Settings::default()
    });
    let (addr, _stop) = start(Arc::clone(&fake)).await;
    let mut hc = cfg();
    hc.unhealthy_threshold = 1;
    hc.timeout = Duration::from_secs(30);
    let eps = vec![endpoint(&addr.to_string()), endpoint(&addr.to_string())];
    let ch = Changes::default();
    let c = Arc::new(Checker::new(
        hc,
        eps.clone(),
        MonotonicClock::shared(),
        ch.callback(),
    ));
    let (shutdown, stopped) = oneshot::channel::<()>();
    let running = tokio::spawn(Arc::clone(&c).run(async move {
        let _ = stopped.await;
    }));
    let deadline = Instant::now() + Duration::from_secs(5);
    while fake.counts().2 < 2 {
        assert!(Instant::now() < deadline, "the probes never arrived");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = shutdown.send(());
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("run waits for no probe")
        .unwrap();
    assert!(
        eps.iter().all(|ep| ep.state.healthy()),
        "a canceled probe marked an endpoint down"
    );
    assert!(ch.list().is_empty());
    fake.close();
}

/// A checker with nothing to probe returns as soon as it is told to stop.
#[tokio::test]
async fn run_with_no_endpoints() {
    let c = Arc::new(Checker::new(
        cfg(),
        Vec::new(),
        MonotonicClock::shared(),
        None,
    ));
    tokio::time::timeout(Duration::from_secs(5), c.run(async {}))
        .await
        .unwrap();
}
