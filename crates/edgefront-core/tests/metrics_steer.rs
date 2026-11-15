//! steer's `/metrics`, against the Rust port: `testdata/metrics_steer.prom`
//! is a scrape of the steer binary taken by
//! `legacy/steer/cmd/steer/metrics_golden_test.go`, after traffic through
//! two clusters with TLS, retries, outlier detection, health checks, a
//! request with no route and two reloads. These tests replay the same
//! events into [`SteerMetrics`] and require the same families, types, help
//! text, series and labels, in the same order, with the same values; only
//! the histogram buckets and sums, which hold the scrape's real latencies,
//! are left out of the comparison.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use edgefront_core::telemetry::metrics::{AttemptResult, HandshakeResult, Registry, SteerMetrics};

const GOLDEN: &str = include_str!("testdata/metrics_steer.prom");

/// The events of the recorded scrape, in [`SteerMetrics`].
fn replay() -> String {
    let m = SteerMetrics::new(Arc::new(Registry::new()), || 2);
    let tls = m.tls(|| 0);
    let mut web = m.cluster("web", &["127.0.0.1:1", "127.0.0.1:19101"]);
    let mut api = m.cluster("api", &["127.0.0.1:19102"]);

    // api's endpoint fails its first probe, then passes.
    m.health_transition("api", false);
    m.health_transition("api", true);
    // Four requests to web: the dead endpoint fails once, is ejected, and
    // the retry and every later request go to the live one.
    tls.handshake(HandshakeResult::Ok, Duration::from_millis(1));
    web.attempt(0, AttemptResult::ConnectError);
    m.outlier_ejection("web", "consecutive_gateway_failure");
    web.retry();
    for _ in 0..4 {
        web.attempt(1, AttemptResult::Success);
        web.request(200, Duration::from_micros(200));
    }
    api.attempt(0, AttemptResult::Http5xx);
    api.request(503, Duration::from_micros(300));
    m.no_route();
    // Plain HTTP on the TLS listener.
    tls.handshake(HandshakeResult::Error, Duration::ZERO);
    // A reload that applies builds both clusters again; one that is
    // refused changes nothing else.
    m.reload_succeeded();
    web = m.cluster("web", &["127.0.0.1:1", "127.0.0.1:19101"]);
    api = m.cluster("api", &["127.0.0.1:19102"]);
    m.reload_failed();
    drop((web, api));
    m.registry().render()
}

/// The exposition with the values of histogram buckets and sums removed.
fn comparable(exposition: &str) -> Vec<String> {
    exposition
        .lines()
        .map(|line| {
            let (series, _) = line.rsplit_once(' ').unwrap_or((line, ""));
            let name = series.split('{').next().unwrap_or(series);
            if !line.starts_with('#') && (name.ends_with("_bucket") || name.ends_with("_sum")) {
                series.to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect()
}

/// Every family named by a `# TYPE` line, with its type.
fn families(exposition: &str) -> BTreeSet<(String, String)> {
    exposition
        .lines()
        .filter_map(|line| line.strip_prefix("# TYPE "))
        .map(|rest| {
            let (name, kind) = rest.split_once(' ').expect("TYPE line has a kind");
            (name.to_owned(), kind.to_owned())
        })
        .collect()
}

/// Every series name with its label names, values left out.
fn label_sets(exposition: &str) -> BTreeSet<String> {
    exposition
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| {
            let series = line.rsplit_once(' ').map_or(line, |(s, _)| s);
            match series.split_once('{') {
                None => series.to_owned(),
                Some((name, labels)) => {
                    let names: Vec<&str> = labels
                        .trim_end_matches('}')
                        .split(',')
                        .filter_map(|pair| pair.split_once('=').map(|(k, _)| k))
                        .collect();
                    format!("{name}{{{}}}", names.join(","))
                }
            }
        })
        .collect()
}

#[test]
fn the_same_metric_names_as_steer() {
    let ours = replay();
    let theirs = families(GOLDEN);
    assert_eq!(theirs.len(), 14, "the golden scrape holds every family");
    let ours = families(&ours);
    let missing: Vec<_> = theirs.difference(&ours).collect();
    let extra: Vec<_> = ours.difference(&theirs).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "families steer has and we lack: {missing:?}; families only we have: {extra:?}"
    );
}

#[test]
fn the_same_label_names_as_steer() {
    let ours = label_sets(&replay());
    let theirs = label_sets(GOLDEN);
    assert_eq!(ours, theirs);
    assert!(theirs.contains("steer_upstream_attempts_total{cluster,endpoint,result}"));
    assert!(theirs.contains("steer_request_duration_seconds_bucket{cluster,le}"));
    assert!(theirs.contains("steer_no_route_total"));
}

#[test]
fn the_same_exposition_as_steer() {
    let ours = comparable(&replay());
    let theirs = comparable(GOLDEN);
    for (i, (a, b)) in ours.iter().zip(&theirs).enumerate() {
        assert_eq!(a, b, "line {} differs", i + 1);
    }
    assert_eq!(ours.len(), theirs.len(), "the expositions differ in length");
}

#[test]
fn the_golden_histograms_are_cumulative() {
    // A check on the recording itself: the bucket counts it holds, which
    // the comparison skips, rise to each series' count.
    let mut last: Option<(String, u64)> = None;
    for line in GOLDEN.lines().filter(|l| !l.starts_with('#')) {
        let (series, value) = line.rsplit_once(' ').unwrap();
        if let Some((labels, _)) = series
            .split_once(",le=")
            .or_else(|| series.split_once("{le="))
        {
            let value: u64 = value.parse().unwrap();
            if let Some((prev, n)) = &last
                && prev == labels
            {
                assert!(value >= *n, "{line} falls below {n}");
            }
            last = Some((labels.to_owned(), value));
        }
    }
    assert!(last.is_some());
}
