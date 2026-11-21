//! The proxy track's HTTP/2 and gRPC benchmark: the scenarios in
//! `scenarios/h2_grpc.toml`, the three targets they run against, and both
//! scenarios run in process through the load generator at 1,000
//! concurrent streams against the benchmark origin.

use std::path::{Path, PathBuf};
use std::time::Duration;

use edgefront_bench::load::Protocol;
use edgefront_bench::origin::Origin;
use edgefront_bench::scenario::{Load, RequestKind, Scenario};
use edgefront_bench::targets::{self, LoadOptions, Target};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn scenarios() -> Vec<Scenario> {
    Scenario::load_all(Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios/h2_grpc.toml"))
        .unwrap()
}

#[test]
fn both_scenarios_hold_1000_streams_over_http2() {
    let all = scenarios();
    let names: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["h2-1000-streams", "grpc-1000-streams"]);
    for s in &all {
        assert_eq!(s.protocol, Protocol::Http2, "{}", s.name);
        assert_eq!(s.connections, 10, "{}", s.name);
        assert_eq!(s.origin_delay, Duration::from_millis(200), "{}", s.name);
        let Load::Constant { rate, duration } = s.load else {
            panic!("{}: a constant load", s.name);
        };
        // Little's law: the rate times how long each stream stays open.
        assert_eq!(rate * s.origin_delay.as_secs_f64(), 1000.0, "{}", s.name);
        assert_eq!(duration, Duration::from_secs(30));
        assert!(s.warmup.is_some(), "{}", s.name);
        assert!(
            s.description.contains("1,000 concurrent streams"),
            "{}",
            s.name
        );
        // The origin's 10,000 streams a connection leave room for every
        // stream a proxy multiplexes upstream.
        assert!(
            rate * s.origin_delay.as_secs_f64() < f64::from(edgefront_bench::origin::MAX_STREAMS)
        );
    }
    assert_eq!(all[0].kind, RequestKind::Get);
    assert_eq!(all[0].objects[0].size, 4096);
    assert_eq!(all[1].kind, RequestKind::Grpc);
    assert_eq!(all[1].objects[0].size, 1024);
    // Each can be picked out by name, as the load generator's container
    // does.
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios/h2_grpc.toml");
    assert_eq!(
        Scenario::load_named(&file, "grpc-1000-streams").unwrap(),
        all[1]
    );
}

#[test]
fn the_three_targets_speak_http2_on_both_sides_with_room_for_the_streams() {
    let dir = root().join("bench/targets");
    for (name, config, product) in [
        ("edgefront-proxy-h2", "proxy.json", "edgefront proxy"),
        ("envoy-h2", "envoy.yaml", "Envoy"),
        ("haproxy-h2", "haproxy.cfg", "HAProxy"),
    ] {
        let target = Target::load(&dir.join(name)).unwrap();
        assert_eq!(target.product, product);
        assert_eq!(
            (target.kind.as_str(), target.track.as_str()),
            ("proxy", "proxy"),
            "{name}"
        );
        let text = std::fs::read_to_string(dir.join(name).join(config)).unwrap();
        // Each lets one connection carry at least the 100 streams a load
        // connection brings, on both sides.
        match name {
            "edgefront-proxy-h2" => {
                let json: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(json["http2"]["cleartext"], true);
                assert_eq!(json["http2"]["max_concurrent_streams"], 1000);
                assert_eq!(json["clusters"][0]["protocol"], "http2");
                // Past the 1,000 streams, as the others' limits are.
                assert!(json["clusters"][0]["max_requests"].as_u64().unwrap() > 1000);
            }
            "envoy-h2" => {
                assert_eq!(text.matches("max_concurrent_streams: 1000").count(), 2);
                assert!(text.contains("explicit_http_config"));
                assert!(!text.contains("envoy.filters.http.cache"));
            }
            _ => {
                assert!(text.contains("tune.h2.fe.max-concurrent-streams 1000"));
                assert!(text.contains("tune.h2.be.max-concurrent-streams 1000"));
                assert!(text.contains("proto h2"));
                assert!(!text.contains("cache-use"));
            }
        }
    }
}

/// Both scenarios, shortened to a second of warm-up and two of load, run
/// straight against the origin: every GET and every gRPC call is answered,
/// and about 1,000 streams are open at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_scenario_keeps_1000_streams_open_against_the_origin() {
    for mut s in scenarios() {
        s.warmup = Some((1000.0, Duration::from_secs(1)));
        s.load = Load::Constant {
            rate: 5000.0,
            duration: Duration::from_secs(2),
        };
        let origin = Origin::bind_with_delay("127.0.0.1:0".parse().unwrap(), s.origin_delay)
            .await
            .unwrap();
        let addr = origin.addr().to_string();
        let options = LoadOptions {
            target: addr.clone(),
            origin: addr,
            cgroup_root: PathBuf::from("/nonexistent"),
            cgroups: vec![],
            ready_within: Duration::from_secs(10),
        };
        let outcome = targets::load_and_measure(&s, &options).await.unwrap();
        let m = &outcome.measured;
        assert_eq!(m.scheduled, 10_000, "{}", s.name);
        assert_eq!(m.completed, 10_000, "{}: {:?}", s.name, m.errors);
        assert_eq!(m.errors.grpc, 0, "{}", s.name);
        assert_eq!(m.status_classes[1], 10_000, "{}", s.name);
        assert!(
            (950..=1200).contains(&m.peak_in_flight),
            "{}: {} streams at once",
            s.name,
            m.peak_in_flight
        );
        // Each answer took the origin's 200 ms and little more.
        assert!(
            m.latency.p50_ms >= 200.0,
            "{}: {}",
            s.name,
            m.latency.p50_ms
        );
        // The body is the object, plus each gRPC message's 5-byte prefix.
        let per = match s.kind {
            RequestKind::Get => 4096,
            RequestKind::Grpc => 1024 + 5,
        };
        assert_eq!(m.body_bytes, 10_000 * per, "{}", s.name);
    }
}
