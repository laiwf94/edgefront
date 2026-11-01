//! Counters and latency histograms on relaxed atomics, and a writer for
//! the Prometheus text exposition format (version 0.0.4).
//!
//! Each product keeps its own fixed set of metric families in a struct of
//! atomics and renders it with these helpers; a handful of families does
//! not justify a metrics library or a dynamic registry on the hot path.

use std::fmt::{self, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The `Content-Type` of a rendered exposition.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4";

/// The kind of a metric family, for its `# TYPE` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Counter,
    Gauge,
    Histogram,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Writes the `# HELP` and `# TYPE` lines that open a family. Samples
/// with labels follow with `writeln!`.
pub fn family(out: &mut String, name: &str, help: &str, kind: Kind) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
}

/// Writes a counter family with a single unlabelled sample.
pub fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    family(out, name, help, Kind::Counter);
    let _ = writeln!(out, "{name} {value}");
}

/// Writes a gauge family with a single unlabelled sample.
pub fn gauge(out: &mut String, name: &str, help: &str, value: u64) {
    family(out, name, help, Kind::Gauge);
    let _ = writeln!(out, "{name} {value}");
}

/// Upper bounds of the latency buckets, in seconds: 100 µs to 10 s.
pub const BUCKETS: [f64; 14] = [
    0.0001, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 10.0,
];

/// A latency histogram over [`BUCKETS`], with the sum kept in whole
/// microseconds.
#[derive(Debug, Default)]
pub struct Histogram {
    counts: [AtomicU64; BUCKETS.len() + 1],
    sum_micros: AtomicU64,
}

impl Histogram {
    /// Records one observation.
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

    /// Observations so far.
    pub fn count(&self) -> u64 {
        self.counts.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }

    /// Writes the cumulative buckets, sum and count of family `name`. The
    /// caller writes the family's header first.
    pub fn render(&self, out: &mut String, name: &str) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families() {
        let mut out = String::new();
        counter(&mut out, "x_total", "Things.", 3);
        gauge(&mut out, "x_open", "Open things.", 1);
        family(&mut out, "x_by", "By label.", Kind::Counter);
        assert_eq!(
            out,
            "# HELP x_total Things.\n# TYPE x_total counter\nx_total 3\n\
             # HELP x_open Open things.\n# TYPE x_open gauge\nx_open 1\n\
             # HELP x_by By label.\n# TYPE x_by counter\n"
        );
        assert_eq!(Kind::Histogram.to_string(), "histogram");
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let h = Histogram::default();
        h.observe(Duration::from_micros(100)); // on the first bound
        h.observe(Duration::from_micros(101));
        h.observe(Duration::from_millis(30));
        h.observe(Duration::from_secs(60)); // past the last bound
        assert_eq!(h.count(), 4);
        let mut out = String::new();
        h.render(&mut out, "lat");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), BUCKETS.len() + 3);
        assert_eq!(lines[0], "lat_bucket{le=\"0.0001\"} 1");
        assert_eq!(lines[1], "lat_bucket{le=\"0.0005\"} 2");
        assert_eq!(lines[6], "lat_bucket{le=\"0.025\"} 2");
        assert_eq!(lines[7], "lat_bucket{le=\"0.05\"} 3");
        assert_eq!(lines[13], "lat_bucket{le=\"10\"} 3");
        assert_eq!(lines[14], "lat_bucket{le=\"+Inf\"} 4");
        assert_eq!(lines[15], "lat_sum 60.030201");
        assert_eq!(lines[16], "lat_count 4");
    }

    #[test]
    fn huge_durations_saturate_the_sum() {
        let h = Histogram::default();
        h.observe(Duration::MAX);
        let mut out = String::new();
        h.render(&mut out, "lat");
        assert!(out.contains("lat_count 1"));
        assert!(out.contains(&format!("lat_sum {}", u64::MAX as f64 / 1e6)));
    }
}
