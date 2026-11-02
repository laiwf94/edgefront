//! cachet's metric families: counters and a latency histogram on relaxed
//! atomics, rendered in the Prometheus text exposition format with the
//! helpers in `edgefront_core::metrics`.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub use edgefront_core::metrics::Histogram;
use edgefront_core::metrics::{Kind, counter, family, gauge};

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

        family(
            &mut out,
            "cachet_requests_total",
            "Client requests by how they were answered.",
            Kind::Counter,
        );
        for (o, n) in Outcome::ALL.iter().zip(&self.requests) {
            let _ = writeln!(
                out,
                "cachet_requests_total{{outcome=\"{}\"}} {}",
                o.as_str(),
                get(n)
            );
        }
        family(
            &mut out,
            "cachet_not_stored_total",
            "Origin responses not stored, by reason.",
            Kind::Counter,
        );
        for (r, n) in NOT_STORABLE.iter().zip(&self.not_stored) {
            let _ = writeln!(
                out,
                "cachet_not_stored_total{{reason=\"{}\"}} {}",
                r.as_str(),
                get(n)
            );
        }
        family(
            &mut out,
            "cachet_request_duration_seconds",
            "Time to answer client requests.",
            Kind::Histogram,
        );
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

        family(
            &mut out,
            "cachet_origin_up",
            "Whether the origin is currently considered healthy.",
            Kind::Gauge,
        );
        for o in pool.origins() {
            let _ = writeln!(
                out,
                "cachet_origin_up{{origin=\"{}\"}} {}",
                o.name,
                u8::from(o.is_healthy())
            );
        }
        family(
            &mut out,
            "cachet_origin_requests_total",
            "Requests sent to each origin.",
            Kind::Counter,
        );
        for o in pool.origins() {
            let _ = writeln!(
                out,
                "cachet_origin_requests_total{{origin=\"{}\"}} {}",
                o.name,
                get(&o.requests)
            );
        }
        family(
            &mut out,
            "cachet_origin_errors_total",
            "Requests to each origin that failed without a response.",
            Kind::Counter,
        );
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
    use crate::store::{CacheSettings, DiskSettings};

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("cachet-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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
