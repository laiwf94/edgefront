//! Counters and a latency histogram on relaxed atomics, rendered in the
//! Prometheus text exposition format. A handful of fixed metric families
//! does not justify a metrics library.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::origin::OriginPool;
use crate::policy::NotStorable;
use crate::store::Cache;

/// How a request was answered, for the requests counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Fresh from the cache.
    Hit,
    /// Stale from the cache while a refresh runs, or accepted by max-stale.
    Stale,
    /// Stale from the cache because the origin failed.
    StaleIfError,
    /// Fetched from the origin and possibly stored.
    Miss,
    /// Revalidated with a 304.
    Revalidated,
    /// Not cacheable: passed through to the origin.
    Pass,
    /// Answered with an error the proxy generated.
    Error,
}

impl Outcome {
    const ALL: [Outcome; 7] = [
        Outcome::Hit,
        Outcome::Stale,
        Outcome::StaleIfError,
        Outcome::Miss,
        Outcome::Revalidated,
        Outcome::Pass,
        Outcome::Error,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Hit => "hit",
            Outcome::Stale => "stale",
            Outcome::StaleIfError => "stale_if_error",
            Outcome::Miss => "miss",
            Outcome::Revalidated => "revalidated",
            Outcome::Pass => "pass",
            Outcome::Error => "error",
        }
    }
}

const NOT_STORABLE: [NotStorable; 9] = [
    NotStorable::Method,
    NotStorable::RequestNoStore,
    NotStorable::Status,
    NotStorable::NoStore,
    NotStorable::Private,
    NotStorable::Authorization,
    NotStorable::VaryStar,
    NotStorable::SetCookie,
    NotStorable::NeverReusable,
];

/// Upper bounds of the latency buckets, in seconds.
const BUCKETS: [f64; 14] = [
    0.0001, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 10.0,
];

#[derive(Debug, Default)]
pub struct Histogram {
    counts: [AtomicU64; BUCKETS.len() + 1],
    sum_micros: AtomicU64,
}

impl Histogram {
    pub fn observe(&self, d: Duration) {
        let secs = d.as_secs_f64();
        let i = BUCKETS
            .iter()
            .position(|&b| secs <= b)
            .unwrap_or(BUCKETS.len());
        self.counts[i].fetch_add(1, Ordering::Relaxed);
        let micros = u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
    }

    fn render(&self, out: &mut String, name: &str) {
        let mut cumulative = 0;
        for (i, bound) in BUCKETS.iter().enumerate() {
            cumulative += self.counts[i].load(Ordering::Relaxed);
            let _ = writeln!(out, "{name}_bucket{{le=\"{bound}\"}} {cumulative}");
        }
        cumulative += self.counts[BUCKETS.len()].load(Ordering::Relaxed);
        let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {cumulative}");
        let sum = self.sum_micros.load(Ordering::Relaxed) as f64 / 1e6;
        let _ = writeln!(out, "{name}_sum {sum}");
        let _ = writeln!(out, "{name}_count {cumulative}");
    }
}

#[derive(Debug, Default)]
pub struct Metrics {
    requests: [AtomicU64; Outcome::ALL.len()],
    not_stored: [AtomicU64; NOT_STORABLE.len()],
    pub collapsed: AtomicU64,
    pub background_refreshes: AtomicU64,
    pub streamed_uncached: AtomicU64,
    pub purges_local: AtomicU64,
    pub purges_peer: AtomicU64,
    pub purged_objects: AtomicU64,
    pub gossip_sent: AtomicU64,
    pub gossip_retries: AtomicU64,
    pub gossip_dropped: AtomicU64,
    pub gossip_rejected: AtomicU64,
    pub latency: Histogram,
}

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(
        out,
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}"
    );
}

fn gauge(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(
        out,
        "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}"
    );
}

impl Metrics {
    pub fn request(&self, outcome: Outcome, elapsed: Duration) {
        let i = Outcome::ALL
            .iter()
            .position(|o| *o == outcome)
            .expect("listed");
        self.requests[i].fetch_add(1, Ordering::Relaxed);
        self.latency.observe(elapsed);
    }

    pub fn requests(&self, outcome: Outcome) -> u64 {
        let i = Outcome::ALL
            .iter()
            .position(|o| *o == outcome)
            .expect("listed");
        self.requests[i].load(Ordering::Relaxed)
    }

    pub fn not_stored(&self, reason: NotStorable) {
        let i = NOT_STORABLE
            .iter()
            .position(|r| *r == reason)
            .expect("listed");
        self.not_stored[i].fetch_add(1, Ordering::Relaxed);
    }

    /// Renders every family in the text exposition format.
    pub fn render(&self, cache: &Cache, pool: &OriginPool, in_flight: usize) -> String {
        let mut out = String::with_capacity(4096);
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);

        out.push_str("# HELP cachet_requests_total Client requests by how they were answered.\n");
        out.push_str("# TYPE cachet_requests_total counter\n");
        for (o, n) in Outcome::ALL.iter().zip(&self.requests) {
            let _ = writeln!(
                out,
                "cachet_requests_total{{outcome=\"{}\"}} {}",
                o.as_str(),
                get(n)
            );
        }
        out.push_str("# HELP cachet_not_stored_total Origin responses not stored, by reason.\n");
        out.push_str("# TYPE cachet_not_stored_total counter\n");
        for (r, n) in NOT_STORABLE.iter().zip(&self.not_stored) {
            let _ = writeln!(
                out,
                "cachet_not_stored_total{{reason=\"{}\"}} {}",
                r.as_str(),
                get(n)
            );
        }
        out.push_str("# HELP cachet_request_duration_seconds Time to answer client requests.\n");
        out.push_str("# TYPE cachet_request_duration_seconds histogram\n");
        self.latency
            .render(&mut out, "cachet_request_duration_seconds");

        counter(
            &mut out,
            "cachet_collapsed_requests_total",
            "Requests that waited on another request's origin fetch.",
            get(&self.collapsed),
        );
        counter(
            &mut out,
            "cachet_background_refreshes_total",
            "Stale-while-revalidate refreshes started.",
            get(&self.background_refreshes),
        );
        counter(
            &mut out,
            "cachet_streamed_uncached_total",
            "Responses too large to buffer, streamed without caching.",
            get(&self.streamed_uncached),
        );
        counter(
            &mut out,
            "cachet_purges_local_total",
            "Purge requests received through the admin API.",
            get(&self.purges_local),
        );
        counter(
            &mut out,
            "cachet_purges_peer_total",
            "Purge messages applied from peers.",
            get(&self.purges_peer),
        );
        counter(
            &mut out,
            "cachet_purged_objects_total",
            "URLs removed by purges.",
            get(&self.purged_objects),
        );
        counter(
            &mut out,
            "cachet_gossip_sent_total",
            "Purge messages delivered to peers.",
            get(&self.gossip_sent),
        );
        counter(
            &mut out,
            "cachet_gossip_retries_total",
            "Purge deliveries retried after a failure.",
            get(&self.gossip_retries),
        );
        counter(
            &mut out,
            "cachet_gossip_dropped_total",
            "Purge deliveries abandoned after the retry window or a full queue.",
            get(&self.gossip_dropped),
        );
        counter(
            &mut out,
            "cachet_gossip_rejected_total",
            "Peer purge messages rejected: bad signature, stale or replayed.",
            get(&self.gossip_rejected),
        );

        let s = &cache.stats;
        counter(
            &mut out,
            "cachet_memory_evictions_total",
            "Entries evicted from the memory tier.",
            get(&s.memory_evictions),
        );
        counter(
            &mut out,
            "cachet_disk_hits_total",
            "Lookups answered by the disk tier.",
            get(&s.disk_hits),
        );
        counter(
            &mut out,
            "cachet_disk_writes_total",
            "Entries written to the disk tier.",
            get(&s.disk_writes),
        );
        counter(
            &mut out,
            "cachet_disk_write_errors_total",
            "Disk tier writes that failed.",
            get(&s.disk_write_errors),
        );
        counter(
            &mut out,
            "cachet_disk_queue_full_total",
            "Disk tier writes skipped because the queue was full.",
            get(&s.disk_queue_full),
        );
        gauge(
            &mut out,
            "cachet_memory_bytes",
            "Bytes held by the memory tier.",
            cache.memory().bytes() as u64,
        );
        gauge(
            &mut out,
            "cachet_memory_capacity_bytes",
            "Byte budget of the memory tier.",
            cache.memory().capacity() as u64,
        );
        gauge(
            &mut out,
            "cachet_memory_objects",
            "URLs held by the memory tier.",
            cache.memory().len() as u64,
        );
        if let Some(disk) = cache.disk() {
            gauge(
                &mut out,
                "cachet_disk_bytes",
                "Bytes held by the disk tier.",
                disk.bytes(),
            );
            gauge(
                &mut out,
                "cachet_disk_objects",
                "URLs held by the disk tier.",
                disk.len() as u64,
            );
        }
        gauge(
            &mut out,
            "cachet_origin_fetches_in_flight",
            "Origin fetches shared through request collapsing.",
            in_flight as u64,
        );

        out.push_str(
            "# HELP cachet_origin_up Whether the origin is currently considered healthy.\n",
        );
        out.push_str("# TYPE cachet_origin_up gauge\n");
        for o in pool.origins() {
            let _ = writeln!(
                out,
                "cachet_origin_up{{origin=\"{}\"}} {}",
                o.name,
                u8::from(o.is_healthy())
            );
        }
        out.push_str("# HELP cachet_origin_requests_total Requests sent to each origin.\n");
        out.push_str("# TYPE cachet_origin_requests_total counter\n");
        for o in pool.origins() {
            let _ = writeln!(
                out,
                "cachet_origin_requests_total{{origin=\"{}\"}} {}",
                o.name,
                get(&o.requests)
            );
        }
        out.push_str("# HELP cachet_origin_errors_total Requests to each origin that failed without a response.\n");
        out.push_str("# TYPE cachet_origin_errors_total counter\n");
        for o in pool.origins() {
            let _ = writeln!(
                out,
                "cachet_origin_errors_total{{origin=\"{}\"}} {}",
                o.name,
                get(&o.errors)
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::origin::{HealthSettings, Origin, client};
    use crate::store::disk::tests::TempDir;
    use crate::store::{CacheSettings, DiskSettings};

    #[tokio::test]
    async fn every_rendered_line_parses() {
        let m = Metrics::default();
        m.request(Outcome::Hit, Duration::from_micros(50));
        m.request(Outcome::Miss, Duration::from_millis(30));
        m.request(Outcome::Error, Duration::from_secs(60));
        m.not_stored(NotStorable::Private);
        let dir = TempDir::new("metrics");
        let cache = Cache::new(&CacheSettings {
            disk: Some(DiskSettings {
                path: dir.0.clone(),
                max_bytes: 1 << 20,
                queue: 4,
            }),
            ..CacheSettings::default()
        })
        .unwrap();
        let pool = OriginPool::new(
            vec![Arc::new(Origin::new(
                "primary",
                "127.0.0.1:1".parse().unwrap(),
            ))],
            client(Duration::from_secs(1)),
            Duration::from_secs(1),
            HealthSettings::default(),
        );
        let text = m.render(&cache, &pool, 2);
        for line in text.lines() {
            if line.starts_with('#') {
                assert!(
                    line.starts_with("# HELP cachet_") || line.starts_with("# TYPE cachet_"),
                    "{line}"
                );
                continue;
            }
            let (series, value) = line.rsplit_once(' ').unwrap();
            assert!(series.starts_with("cachet_"), "{line}");
            assert!(value.parse::<f64>().is_ok(), "{line}");
        }
        assert!(text.contains("cachet_requests_total{outcome=\"hit\"} 1"));
        assert!(text.contains("cachet_not_stored_total{reason=\"private\"} 1"));
        assert!(text.contains("cachet_request_duration_seconds_bucket{le=\"0.0001\"} 1"));
        assert!(text.contains("cachet_request_duration_seconds_bucket{le=\"+Inf\"} 3"));
        assert!(text.contains("cachet_request_duration_seconds_count 3"));
        assert!(text.contains("cachet_origin_up{origin=\"primary\"} 1"));
        assert!(text.contains("cachet_disk_objects 0"));
        assert!(text.contains("cachet_origin_fetches_in_flight 2"));
        assert_eq!(m.requests(Outcome::Hit), 1);
    }
}
