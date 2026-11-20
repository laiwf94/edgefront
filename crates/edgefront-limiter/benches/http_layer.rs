//! What HTTP limiting adds to a request in the proxy pipeline: done-when
//! item 4 of week 6 (under 2 microseconds at p99).
//!
//! Each request goes through `edgefront_core::filter::run`, the proxy's
//! filter phases around an upstream call, twice: once on a route with the
//! limiter's filter and once on a route without one, alternately, so both
//! see the same machine noise. The upstream is an immediately ready
//! future that answers `200`, so what differs is the limiter's work: the
//! key built from the path and an API key header, the table take, the
//! settlement, and the `RateLimit-Policy` and `RateLimit` headers on the
//! response. The rule is keyed on path and header with 100,000 distinct
//! keys (1,000 paths by 100 API keys) in a table of 262,144, at 100 a
//! second with a burst of 100, so nearly every request is admitted and
//! takes the full path; the clock is the real monotonic one.
//!
//! Every request is timed on its own (the clock ticks every 41.7 ns on
//! Apple silicon, well under the microsecond scale measured). The p99 of
//! the added time is reported as the p99 with the limiter less the p99
//! without, and as the p99 of the per-request differences of each pair.
//!
//! Run with `cargo bench -p edgefront-limiter --bench http_layer`.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use edgefront_core::body::{Body, full};
use edgefront_core::filter::{Delivery, RouteFilters, run};
use edgefront_limiter::clock::MonotonicClock;
use edgefront_limiter::config::http_limits::{HttpLimitsSection, build};
use edgefront_limiter::http::HttpLimiter;
use http::{Request, Response};

const REQUESTS: usize = 1_000_000;
const WARMUP: usize = 200_000;
const PATHS: usize = 1000;
const API_KEYS: usize = 100;

const RULES: &str = r#"
[[http_limits]]
name = "api"
rate = "100/s burst 100"
key = ["path", "header:x-api-key"]
table_size = 262144
"#;

fn request(i: usize) -> Request<()> {
    Request::get(format!("/api/items/{}", i % PATHS))
        .header("host", "bench.test")
        .header("x-api-key", format!("key-{:04}", (i / PATHS) % API_KEYS))
        .body(())
        .unwrap()
}

async fn upstream(_req: Request<()>) -> (Response<Body>, Delivery) {
    (Response::new(full("ok")), Delivery::Upstream)
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let section: HttpLimitsSection = toml::from_str(RULES).unwrap();
    let limiter = Arc::new(
        HttpLimiter::new(
            build(&section.http_limits).unwrap(),
            Arc::new(MonotonicClock::new()),
        )
        .unwrap(),
    );
    let limited: RouteFilters = limiter.route_filters(1).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (with, without, diffs) = rt.block_on(async {
        let mut with = Vec::with_capacity(REQUESTS);
        let mut without = Vec::with_capacity(REQUESTS);
        let mut diffs = Vec::with_capacity(REQUESTS);
        for i in 0..WARMUP + REQUESTS {
            let (a, b) = (request(i), request(i));
            let t0 = Instant::now();
            let r = run(limited.get(0), 0, a, upstream).await;
            let t1 = Instant::now();
            black_box(r);
            let t2 = Instant::now();
            let r = run(None, 0, b, upstream).await;
            let t3 = Instant::now();
            black_box(r);
            if i >= WARMUP {
                let w = (t1 - t0).as_nanos() as u64;
                let o = (t3 - t2).as_nanos() as u64;
                with.push(w);
                without.push(o);
                diffs.push(w.saturating_sub(o));
            }
        }
        (with, without, diffs)
    });
    let mut with = with;
    let mut without = without;
    let mut diffs = diffs;
    with.sort_unstable();
    without.sort_unstable();
    diffs.sort_unstable();
    let s = &limiter.stats()[0];
    println!(
        "{REQUESTS} requests after {WARMUP} of warm-up; {} admitted, {} refused, {} keys",
        s.allowed, s.refused, s.keys
    );
    println!("| ns per request | p50 | p90 | p99 | p99.9 |");
    println!("|---|---:|---:|---:|---:|");
    for (name, v) in [
        ("without the limiter", &without),
        ("with the limiter", &with),
        ("paired difference", &diffs),
    ] {
        println!(
            "| {name} | {} | {} | {} | {} |",
            pct(v, 0.5),
            pct(v, 0.9),
            pct(v, 0.99),
            pct(v, 0.999)
        );
    }
    println!(
        "added at p99: {} ns (p99 with less p99 without)",
        pct(&with, 0.99).saturating_sub(pct(&without, 0.99))
    );
}
