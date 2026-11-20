//! HTTP limits' accuracy: what a run through a limited proxy should have
//! refused, held against what it did.
//!
//! A scenario's requests are a pure function of their sequence numbers
//! and each is due at a fixed time ([`crate::load`]), so the decisions a
//! limit should make on a run are known before it starts. [`expected`]
//! replays every request of the scenario, warm-up included, through the
//! target's own `[[http_limits]]` rules (the same
//! [`edgefront_limiter::http::HttpLimiter`] the proxy runs) on a manual
//! clock set to each request's due time, and counts the measured
//! requests it admits and refuses. [`compare`] puts a run's result beside
//! it: its 2xx are the admitted requests and its 4xx the refused ones,
//! since the origin answers nothing else.
//!
//! The real run's requests arrive a little off their due times (queueing
//! in the load generator, the network, the proxy's workers), so a path
//! near its limit can see a token more or less; the comparison measures
//! how much that moves the totals.

use std::sync::Arc;
use std::time::Duration;

use edgefront_core::filter::Decision;
use edgefront_limiter::clock::ManualClock;
use edgefront_limiter::config::http_limits::{HttpLimitsSection, build};
use edgefront_limiter::http::HttpLimiter;
use http::{HeaderMap, HeaderValue, Method, Uri};
use serde::Serialize;

use crate::load::RequestSource;
use crate::report::BenchResult;
use crate::scenario::Scenario;

/// What the rules decide on the scenario's measured requests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Expected {
    pub scenario: String,
    pub warmup_requests: u64,
    pub measured_requests: u64,
    pub admitted: u64,
    pub refused: u64,
    /// Keys the rules' tables hold at the end.
    pub keys: u64,
}

/// One result beside the expectation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Comparison {
    pub target: String,
    pub completed: u64,
    pub admitted: u64,
    pub refused: u64,
    /// Requests that failed or got another status.
    pub other: u64,
    /// How far the admitted count is from the expected one, as a
    /// percentage of the expected.
    pub admitted_error_pct: f64,
    /// The same for the refused count.
    pub refused_error_pct: f64,
}

/// Replays `scenario` through the `[[http_limits]]` rules in `rules`
/// (TOML), every request a `GET` of its path with `Host: host` on route
/// 0.
pub fn expected(scenario: &Scenario, rules: &str, host: &str) -> Result<Expected, String> {
    let section: HttpLimitsSection = toml::from_str(rules).map_err(|e| e.to_string())?;
    let rules = build(&section.http_limits)?;
    let clock = Arc::new(ManualClock::new(0));
    let limiter = Arc::new(HttpLimiter::new(rules, clock.clone())?);
    let all: Vec<usize> = (0..limiter.rules().count()).collect();
    let requests = scenario.requests();
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::HOST,
        HeaderValue::from_str(host).map_err(|e| e.to_string())?,
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|e| e.to_string())?;
    let mut out = Expected {
        scenario: scenario.name.clone(),
        warmup_requests: 0,
        measured_requests: 0,
        admitted: 0,
        refused: 0,
        keys: 0,
    };
    let mut seq = 0u64;
    // Phases start where the one before ends; a clock that starts at a
    // second keeps every timestamp positive.
    let mut start = Duration::from_secs(1);
    for phase in scenario.schedule().phases() {
        for k in 0..phase.requests() {
            let due = start + Duration::from_secs_f64(k as f64 / phase.rate);
            clock.set(due.as_nanos() as u64);
            let req = requests.request(seq);
            let uri: Uri = req.uri().clone();
            let d = rt.block_on(limiter.decide(&all, &Method::GET, &uri, &headers));
            seq += 1;
            if phase.warmup {
                out.warmup_requests += 1;
                continue;
            }
            out.measured_requests += 1;
            match d {
                Decision::Respond(_) => out.refused += 1,
                _ => out.admitted += 1,
            }
        }
        start += phase.duration;
    }
    out.keys = limiter.stats().iter().map(|s| s.keys).sum();
    Ok(out)
}

fn error_pct(got: u64, want: u64) -> f64 {
    if want == 0 {
        return if got == 0 { 0.0 } else { 100.0 };
    }
    (got as f64 - want as f64).abs() * 100.0 / want as f64
}

/// A result against the expectation.
pub fn compare(expected: &Expected, result: &BenchResult) -> Comparison {
    let m = &result.measured;
    let admitted = m.status_classes[1];
    let refused = m.status_classes[3];
    Comparison {
        target: result.target.name.clone(),
        completed: m.completed,
        admitted,
        refused,
        other: m.scheduled.saturating_sub(admitted + refused),
        admitted_error_pct: error_pct(admitted, expected.admitted),
        refused_error_pct: error_pct(refused, expected.refused),
    }
}

/// The expectation and comparisons as a Markdown table.
pub fn markdown(expected: &Expected, rows: &[Comparison]) -> String {
    let mut out = format!(
        "{}: {} measured requests after {} of warm-up, {} keys\n\n\
         | | admitted (2xx) | refused (429) | other | admitted off by | refused off by |\n\
         |---|---:|---:|---:|---:|---:|\n\
         | expected | {} | {} | 0 | | |\n",
        expected.scenario,
        expected.measured_requests,
        expected.warmup_requests,
        expected.keys,
        expected.admitted,
        expected.refused
    );
    for r in rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {:.3}% | {:.3}% |\n",
            r.target, r.admitted, r.refused, r.other, r.admitted_error_pct, r.refused_error_pct
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCENARIO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scenarios/http_limits.toml");
    const RULES: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../bench/targets/edgefront-proxy-limits/limits.toml"
    );

    #[test]
    fn the_scenario_has_its_keys_and_rate() {
        let s = Scenario::load(SCENARIO).unwrap();
        assert_eq!(s.keys.count, 100_000);
        assert_eq!(s.measured_requests(), 1_500_000);
        assert_eq!(s.warmup_requests(), 50_000);
        // About 1% of measured requests go to once-only paths.
        assert!((s.once_only() - 0.01).abs() < 0.005, "{}", s.once_only());
    }

    /// A small scenario replayed exactly: one path at 100 a second for 2
    /// s against 20 a second with a burst of 20 admits the burst and 20 a
    /// second after it.
    #[test]
    fn a_replay_counts_what_the_rules_decide() {
        let s = Scenario::from_toml(
            "name = \"t\"\n[load]\nrate = 100\nduration = \"2s\"\n[keys]\ncount = 1\nzipf = 0.0\nhit_ratio = 0.995\n[[objects]]\nsize = \"1KiB\"\n",
        )
        .unwrap();
        let rules = std::fs::read_to_string(RULES).unwrap();
        let e = expected(&s, &rules, "origin").unwrap();
        assert_eq!(e.measured_requests, 200);
        assert_eq!(e.warmup_requests, 0);
        // 20 at once, then one every 50 ms: at 50, 100, ... 1950 ms.
        assert_eq!(e.admitted, 20 + 39);
        assert_eq!(e.refused, 200 - 59);
        assert_eq!(e.keys, 1);
        let md = markdown(&e, &[]);
        assert!(md.contains("| expected | 59 | 141 | 0 | | |"), "{md}");
        assert_eq!(error_pct(101, 100), 1.0);
        assert_eq!(error_pct(0, 0), 0.0);
        assert_eq!(error_pct(1, 0), 100.0);
        assert!(expected(&s, "[[http_limits]]\nname = \"x\"\n", "h").is_err());
    }
}
