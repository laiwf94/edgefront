//! Discovery at scale, end to end through a running proxy: a delta feed
//! over HTTP/2 gives one cluster 100,000 endpoints, then 1,000 changes a
//! second for a while. Reported: how long the first set took to reach the
//! balancer, each change's time from being published to the balancer
//! balancing on it (p50, p99, max), and the process's resident memory
//! before, after the first set and after the changes.
//!
//! `cargo bench -p edgefront-proxy --bench discovery`; `DISCOVERY_ENDPOINTS`
//! and `DISCOVERY_SECONDS` change the size (100,000) and the length of the
//! sustained phase (10 s). Not a criterion benchmark: the numbers are
//! latencies of a pipeline under a steady rate, not a function's time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use edgefront_core::config::Config;
use edgefront_proxy::discovery::Endpoint;
use edgefront_proxy::discovery::delta::{Feed, serve};
use edgefront_proxy::server::Proxy;
use tokio::net::TcpListener;

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// The process's resident set, in KiB, from `ps`.
fn rss_kib() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

fn ep(i: u64) -> Endpoint {
    Endpoint::new(format!(
        "10.{}.{}.{}:8080",
        i / 62_500,
        i / 250 % 250,
        i % 250 + 1
    ))
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

/// Waits until the cluster balances on merged version `at` or later.
async fn until_version(proxy: &Proxy, at: u64, limit: Duration) {
    let deadline = Instant::now() + limit;
    while proxy.discovery_version("bench").unwrap_or(0) < at {
        assert!(Instant::now() < deadline, "version {at} never applied");
        tokio::task::yield_now().await;
    }
}

fn main() {
    let n = env("DISCOVERY_ENDPOINTS", 100_000);
    let seconds = env("DISCOVERY_SECONDS", 10);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(run(n, seconds));
}

async fn run(n: u64, seconds: u64) {
    let rss_start = rss_kib();
    let feed = Feed::new(1 << 20);
    feed.set((0..n).map(ep).collect());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        Arc::clone(&feed),
        Duration::from_secs(1),
        std::future::pending(),
    ));
    let config = format!(
        r#"{{"listen":"127.0.0.1:0","routes":[{{"cluster":"bench"}}],
            "clusters":[{{"name":"bench","policy":"round_robin",
            "discovery":{{"sources":[{{"delta":{{"url":"http://{addr}/v1/bench"}}}}]}}}}]}}"#
    );
    let cfg = Config::from_json(config.as_bytes()).expect("a valid config");
    let proxy = Arc::new(Proxy::new(&cfg));
    let rss_built = rss_kib();
    let started = Instant::now();
    {
        let proxy = Arc::clone(&proxy);
        tokio::spawn(async move { proxy.run_checks(std::future::pending()).await });
    }
    until_version(&proxy, 1, Duration::from_secs(120)).await;
    let first = started.elapsed();
    assert_eq!(proxy.endpoints("bench").unwrap().len() as u64, n);
    let rss_loaded = rss_kib();

    // 1,000 changes a second: each removes one endpoint or adds a new one,
    // so every change moves the merged version by exactly one.
    let base_feed = feed.version();
    let base_merged = proxy.discovery_version("bench").unwrap();
    let total = seconds * 1000;
    let mut published: Vec<Instant> = Vec::with_capacity(total as usize);
    let mut latencies: Vec<Duration> = Vec::with_capacity(total as usize);
    let mut seen = 0u64;
    let mut tick = tokio::time::interval(Duration::from_millis(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    let mut next_new = n;
    for i in 0..total {
        tick.tick().await;
        if i % 2 == 0 {
            feed.remove(&ep(i / 2).address);
        } else {
            feed.upsert(ep(next_new));
            next_new += 1;
        }
        published.push(Instant::now());
        // Collect what has been applied so far, without stopping the rate.
        let applied = proxy.discovery_version("bench").unwrap() - base_merged;
        while seen < applied {
            latencies.push(published[seen as usize].elapsed());
            seen += 1;
        }
    }
    assert_eq!(feed.version() - base_feed, total);
    while seen < total {
        let applied = proxy.discovery_version("bench").unwrap() - base_merged;
        let now = Instant::now();
        while seen < applied {
            latencies.push(now - published[seen as usize]);
            seen += 1;
        }
        tokio::task::yield_now().await;
    }
    let rss_end = rss_kib();
    let rate = total as f64
        / published
            .last()
            .unwrap()
            .duration_since(published[0])
            .as_secs_f64();
    latencies.sort();
    let mb = |kib: u64| kib as f64 / 1024.0;
    println!("discovery: {n} endpoints, then {total} changes over {seconds} s ({rate:.0}/s)");
    println!("  first set applied in {:.1} ms", first.as_secs_f64() * 1e3);
    println!(
        "  change to balancer: p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
        percentile(&latencies, 0.50).as_secs_f64() * 1e3,
        percentile(&latencies, 0.99).as_secs_f64() * 1e3,
        latencies.last().unwrap().as_secs_f64() * 1e3,
    );
    println!(
        "  rss: {:.1} MiB at start, {:.1} MiB built, {:.1} MiB with the set, {:.1} MiB after the changes ({:+.1} MiB)",
        mb(rss_start),
        mb(rss_built),
        mb(rss_loaded),
        mb(rss_end),
        mb(rss_end) - mb(rss_loaded),
    );
}
