//! The scenarios checked into bench/scenarios/, and a scenario run end to
//! end through the load generator against an origin that serves
//! `/obj/<size>/<name>`.

use std::collections::HashSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use edgefront_bench::load::{self, LoadConfig, Protocol};
use edgefront_bench::scenario::{Load, Scenario};
use http::{Request, Response};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

fn scenarios_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bench/scenarios")
}

#[test]
fn the_five_checked_in_scenarios_are_valid() {
    let all = Scenario::load_dir(scenarios_dir()).unwrap();
    let names: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "cache-hit",
            "large-objects",
            "miss-heavy",
            "ramp",
            "web-mix"
        ]
    );
    for s in &all {
        // Each reaches its target and is described.
        assert!(
            (s.expected_hit_ratio() - s.keys.hit_ratio).abs() < 0.005,
            "{}: {} vs {}",
            s.name,
            s.expected_hit_ratio(),
            s.keys.hit_ratio
        );
        assert!(!s.description.is_empty(), "{}", s.name);
        assert!(s.warmup.is_some(), "{} warms up", s.name);
        // The file is named after the scenario.
        assert!(scenarios_dir().join(format!("{}.toml", s.name)).exists());
        // Every target keeps the working set in memory: at most 1 GiB.
        assert!(
            s.working_set() <= 1 << 30,
            "{}: {}",
            s.name,
            s.working_set()
        );
    }
    // The demo's scenario runs against two targets in under five minutes.
    let hit = &all[0];
    assert!(hit.schedule().duration() * 2 < Duration::from_secs(240));
    // One ramp, over HTTP/2.
    let ramp = &all[3];
    assert!(matches!(ramp.load, Load::Stepped { steps: 8, .. }));
    assert_eq!(ramp.protocol, Protocol::Http2);
}

/// Serves `/obj/<size>/<name>` with a body of `size` bytes and records each
/// path it is asked for.
async fn origin() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let log = Arc::clone(&log);
            let service = service_fn(move |request: Request<Incoming>| {
                let log = Arc::clone(&log);
                async move {
                    let path = request.uri().path().to_owned();
                    let size: usize = path.split('/').nth(2).unwrap().parse().unwrap();
                    log.lock().unwrap().push(path);
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(vec![b'x'; size]))))
                }
            });
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (addr, seen)
}

#[tokio::test]
async fn a_scenario_runs_through_the_load_generator() {
    let scenario = Scenario::from_toml(
        r#"
        name = "short"
        connections = 4
        timeout = "5s"
        seed = 11

        [warmup]
        rate = 500
        duration = "1s"

        [load]
        rate = 1000
        duration = "1s"

        [keys]
        count = 50
        zipf = 1.0
        hit_ratio = 0.7

        [[objects]]
        size = 100
        weight = 1

        [[objects]]
        size = "1KiB"
        weight = 1
        "#,
    )
    .unwrap();
    let (addr, seen) = origin().await;
    let mut config = LoadConfig::new(addr, scenario.protocol, scenario.schedule());
    config.connections = scenario.connections;
    config.timeout = scenario.timeout;
    let requests = scenario.requests();
    let report = load::run(config, requests.clone()).await.unwrap();

    let warmup = report.phases[0].clone();
    let measured = report.measured();
    assert!(warmup.warmup);
    assert_eq!(warmup.scheduled, 500);
    assert_eq!(measured.scheduled, 1000);
    assert_eq!(measured.completed, 1000);
    assert_eq!(measured.errors.total(), 0);

    // The origin saw exactly the scenario's requests, and was sent the
    // bytes each one names.
    let paths = seen.lock().unwrap().clone();
    let expected: Vec<String> = (0..1500).map(|seq| requests.object(seq).path()).collect();
    let mut got = paths.clone();
    got.sort();
    let mut want = expected.clone();
    want.sort();
    assert_eq!(got, want);
    let measured_bytes: u64 = (500..1500).map(|seq| requests.object(seq).size()).sum();
    assert_eq!(measured.body_bytes, measured_bytes);

    // In sequence order, the measured requests repeat earlier ones close to
    // the target: what a cache keeping everything would hit.
    let mut first = HashSet::new();
    let mut hits = 0;
    for (seq, path) in expected.iter().enumerate() {
        if !first.insert(path) && seq >= 500 {
            hits += 1;
        }
    }
    let ratio = hits as f64 / 1000.0;
    assert!(
        (ratio - scenario.expected_hit_ratio()).abs() < 0.06,
        "{ratio} vs {}",
        scenario.expected_hit_ratio()
    );
}
