//! The harness measuring itself, through the same path a container run
//! takes (`targets::load_and_measure`, the load generator's side):
//!
//! - a target that stalls for 100 ms shows the stall in its corrected
//!   response-time p99, while the closed-loop service time hides most of
//!   it: the coordinated-omission correction works end to end;
//! - two identical runs against a target of fixed latency agree within 5%
//!   on throughput and p99, so a difference above that between two
//!   targets is the targets, not the harness. The same holds for real
//!   targets in containers; `docs/benchmarks/` records those repeats.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use edgefront_bench::origin::Origin;
use edgefront_bench::scenario::Scenario;
use edgefront_bench::targets::{LoadOptions, LoadOutcome, load_and_measure};
use http::{Request, Response};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

fn scenario(rate: u32, seconds: u32, connections: u32) -> Scenario {
    Scenario::from_toml(&format!(
        r#"
        name = "self-test"
        description = "The harness against a target it controls."
        connections = {connections}
        timeout = "5s"
        seed = 7
        [warmup]
        rate = {rate}
        duration = "500ms"
        [load]
        rate = {rate}
        duration = "{seconds}s"
        [keys]
        count = 100
        zipf = 1.0
        hit_ratio = 0.0
        [[objects]]
        size = "1KiB"
        weight = 1
        "#
    ))
    .unwrap()
}

/// The two tests measure time, so they take turns rather than share the
/// machine with each other.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A fixed-latency target on a runtime and threads of its own, so the
/// load generator and the target never wait on each other's scheduler.
fn separate_target(delay: Duration) -> (SocketAddr, std::sync::mpsc::Sender<()>) {
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let origin = runtime
            .block_on(Origin::bind_with_delay(
                "127.0.0.1:0".parse().unwrap(),
                delay,
            ))
            .unwrap();
        addr_tx.send(origin.addr()).unwrap();
        let _ = stop_rx.recv();
        drop(origin);
    });
    (addr_rx.recv().unwrap(), stop_tx)
}

fn options(target: SocketAddr, origin: SocketAddr) -> LoadOptions {
    LoadOptions {
        target: target.to_string(),
        origin: origin.to_string(),
        cgroup_root: PathBuf::from("/nonexistent"),
        cgroups: vec![],
        ready_within: Duration::from_secs(5),
    }
}

/// A target that answers every request at once, except that while
/// `stalled` is set it holds each response until it clears.
async fn stalling_target() -> (SocketAddr, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stalled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stalled);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let flag = Arc::clone(&flag);
            let service = service_fn(move |_: Request<Incoming>| {
                let flag = Arc::clone(&flag);
                async move {
                    while flag.load(Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
                }
            });
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (addr, stalled)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalling_target_shows_its_stall_in_p99() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let origin = Origin::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let (target, stalled) = stalling_target().await;
    // 2 s at 500 requests per second over two connections, with one
    // 100 ms stall a second into the measured phase: 50 requests are due
    // during it, 5% of the measured 1,000, so every one of the slowest 1%
    // waited out most of the stall.
    let scenario = scenario(500, 2, 2);
    let stall = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        stalled.store(true, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(100)).await;
        stalled.store(false, Ordering::Relaxed);
    });
    let outcome = load_and_measure(&scenario, &options(target, origin.addr()))
        .await
        .unwrap();
    stall.await.unwrap();
    let m = &outcome.measured;
    assert_eq!(m.completed, 1000, "{m:?}");
    assert_eq!(m.errors.connect + m.errors.timeout + m.errors.other, 0);
    assert!(
        m.latency.p99_ms >= 80.0,
        "response-time p99 {} ms",
        m.latency.p99_ms
    );
    assert!(
        m.service_latency.p99_ms < m.latency.p99_ms,
        "service-time p99 {} ms against response-time p99 {} ms",
        m.service_latency.p99_ms,
        m.latency.p99_ms
    );
    // The target never asked the origin for anything.
    assert_eq!(m.origin_requests, 0);
    assert_eq!(m.hit_ratio, 1.0);
}

fn within(a: f64, b: f64, share: f64) -> bool {
    (a - b).abs() <= share * a.max(b)
}

async fn fixed_latency_run(target: SocketAddr, origin: SocketAddr) -> LoadOutcome {
    // 1,000 requests per second for 8 s against a target that answers each
    // in 100 ms: about 100 in flight on 256 connections, so nothing queues
    // and p99 is the target's 100 ms plus the timer's rounding. 8,000
    // samples put 80 above p99, so one scheduling hiccup of the shared
    // machine cannot move it, and 5% of 100 ms is well above the
    // millisecond the timers jitter by.
    load_and_measure(&scenario(1000, 8, 256), &options(target, origin))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_identical_runs_agree_within_five_percent() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let (target, stop) = separate_target(Duration::from_millis(100));
    let first = fixed_latency_run(target, target).await;
    let second = fixed_latency_run(target, target).await;
    drop(stop);
    for run in [&first, &second] {
        assert_eq!(run.measured.completed, 8000, "{:?}", run.measured);
        assert!(
            run.measured.latency.p99_ms >= 100.0,
            "{:?}",
            run.measured.latency
        );
    }
    let (a, b) = (&first.measured, &second.measured);
    assert!(
        within(a.throughput_rps, b.throughput_rps, 0.05),
        "throughput {} against {}",
        a.throughput_rps,
        b.throughput_rps
    );
    assert!(
        within(a.latency.p99_ms, b.latency.p99_ms, 0.05),
        "p99 {} ms against {} ms",
        a.latency.p99_ms,
        b.latency.p99_ms
    );
    // Every measured request went to the origin, which is the target.
    assert!(a.origin_requests >= 7900 && a.hit_ratio < 0.04, "{a:?}");
}
