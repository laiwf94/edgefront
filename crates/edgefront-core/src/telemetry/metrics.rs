//! steer's metrics, ported from its `internal/metrics` and from the places
//! in `internal/proxy` and `cmd/steer` that register them: a registry of
//! counters, histograms and gauges keyed by series, rendered in the
//! Prometheus text exposition format exactly as steer rendered it, and
//! [`SteerMetrics`], steer's metric families with their names, labels and
//! help text unchanged.
//!
//! # The registry
//!
//! A series is a family name plus its rendered labels, such as
//! `steer_requests_total{cluster="web",code_class="2xx"}`. [`Registry`]
//! hands out one shared [`Counter`] or [`Histogram`] per series, creating
//! it on first use; the request path holds those handles, so it only
//! touches relaxed atomics and the registry's lock is taken when a
//! configuration is applied, when a rare event (an ejection, a health
//! verdict change) names a series for the first time, and on a scrape.
//! Series are never removed, as in steer: after a reload the series of a
//! cluster that is gone keep their last values.
//!
//! [`Registry::write`] gives steer's output byte for byte: counters first,
//! then gauges, then histograms, each group sorted by series key in byte
//! order, with each family's `# HELP` and `# TYPE` lines before its first
//! series. Histograms use steer's fourteen latency bounds from 50 µs to
//! 2.5 s ([`LATENCY_BUCKETS`]) and keep their sum in nanoseconds. Floats
//! are written as Go's `%.6f` with trailing zeros removed. A family's help
//! is the one given when its newest series was created.
//!
//! # steer's families
//!
//! | family | type | labels |
//! |---|---|---|
//! | `steer_config_reloads_total` | counter | `result` = `success`, `failure` |
//! | `steer_no_route_total` | counter | |
//! | `steer_config_version` | gauge | |
//! | `steer_requests_total` | counter | `cluster`, `code_class` = `1xx`..`5xx`, `unknown` |
//! | `steer_request_duration_seconds` | histogram | `cluster` |
//! | `steer_retries_total` | counter | `cluster` |
//! | `steer_retry_budget_exhausted_total` | counter | `cluster` |
//! | `steer_overflow_total` | counter | `cluster` |
//! | `steer_upstream_attempts_total` | counter | `cluster`, `endpoint`, `result` = [`AttemptResult`] |
//! | `steer_outlier_ejections_total` | counter | `cluster`, `reason` |
//! | `steer_health_transitions_total` | counter | `cluster`, `to` = `healthy`, `unhealthy` |
//! | `steer_tls_handshakes_total` | counter | `result` = `ok`, `timeout`, `error` |
//! | `steer_tls_handshake_duration_seconds` | histogram | |
//! | `steer_tls_handshakes_in_progress` | gauge | |
//!
//! The per-cluster series exist from the moment the cluster is built, at
//! zero; ejections and health transitions appear when they first happen.
//! `crates/edgefront-core/tests/metrics_steer.rs` replays the events of a
//! scrape recorded from the steer binary and compares the two.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// The `Content-Type` of a rendered exposition.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4";

/// steer's `DefaultLatencyBuckets`: the upper bounds of every histogram,
/// in seconds, from 50 µs to 2.5 s.
pub const LATENCY_BUCKETS: [f64; 14] = [
    0.00005, 0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 1.0, 2.5,
];

/// A monotonically increasing count.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    /// Adds one.
    pub fn inc(&self) {
        self.add(1);
    }

    /// Adds `n`, wrapping at 2^64 as steer's `atomic.Uint64` does.
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    /// The current count.
    pub fn value(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Durations counted into [`LATENCY_BUCKETS`], with their sum in
/// nanoseconds.
#[derive(Debug, Default)]
pub struct Histogram {
    /// One count per bound, then the count above the last bound.
    counts: [AtomicU64; LATENCY_BUCKETS.len() + 1],
    sum_nanos: AtomicU64,
    total: AtomicU64,
}

impl Histogram {
    /// Records one duration in the first bucket whose bound is at least
    /// its length in seconds.
    pub fn observe(&self, d: Duration) {
        let secs = d.as_secs_f64();
        let i = LATENCY_BUCKETS.partition_point(|&bound| bound < secs);
        self.counts[i].fetch_add(1, Ordering::Relaxed);
        // A Go duration is at most i64::MAX nanoseconds.
        let nanos = u64::try_from(d.as_nanos()).map_or(i64::MAX as u64, |n| n.min(i64::MAX as u64));
        self.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
        self.total.fetch_add(1, Ordering::Relaxed);
    }

    /// Observations so far.
    pub fn count(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

type GaugeFn = Box<dyn Fn() -> f64 + Send + Sync>;

#[derive(Default)]
struct Inner {
    counters: BTreeMap<String, Arc<Counter>>,
    histograms: BTreeMap<String, Arc<Histogram>>,
    gauges: BTreeMap<String, GaugeFn>,
    help: HashMap<String, String>,
}

/// Every series of a process, rendered together on a scrape.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("Registry")
            .field("counters", &inner.counters.len())
            .field("histograms", &inner.histograms.len())
            .field("gauges", &inner.gauges.len())
            .finish()
    }
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Registry {
        Registry::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic elsewhere cannot leave the maps half-updated: every
        // change is one insert.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The counter of family `name` with `labels`, rendered as they are to
    /// appear between the braces (see [`labels`]), created at zero on
    /// first use. `name` must be a valid Prometheus metric name.
    pub fn counter(&self, name: &str, labels: &str, help: &str) -> Arc<Counter> {
        let mut inner = self.lock();
        let inner = &mut *inner;
        Arc::clone(
            inner
                .counters
                .entry(series(name, labels))
                .or_insert_with(|| {
                    inner.help.insert(name.to_owned(), help.to_owned());
                    Arc::default()
                }),
        )
    }

    /// The histogram of family `name` with `labels`, created empty on
    /// first use.
    pub fn histogram(&self, name: &str, labels: &str, help: &str) -> Arc<Histogram> {
        let mut inner = self.lock();
        let inner = &mut *inner;
        Arc::clone(
            inner
                .histograms
                .entry(series(name, labels))
                .or_insert_with(|| {
                    inner.help.insert(name.to_owned(), help.to_owned());
                    Arc::default()
                }),
        )
    }

    /// Registers the unlabelled gauge `name`, read by calling `read` on
    /// every scrape, replacing any gauge of that name. `read` runs under
    /// the registry's lock, so it must not use the registry.
    pub fn gauge(&self, name: &str, help: &str, read: impl Fn() -> f64 + Send + Sync + 'static) {
        let mut inner = self.lock();
        inner.gauges.insert(name.to_owned(), Box::new(read));
        inner.help.insert(name.to_owned(), help.to_owned());
    }

    /// Appends every series in the text exposition format.
    pub fn write(&self, out: &mut String) {
        let inner = self.lock();
        // A family's header goes before its first series, once, as
        // steer's `typed` set kept it.
        let mut typed = HashSet::new();
        let mut header = |out: &mut String, name: &str, kind: &str| {
            if typed.insert(name.to_owned()) {
                let help = inner.help.get(name).map_or("", String::as_str);
                let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
            }
        };
        for (key, c) in &inner.counters {
            let (name, _) = split_series(key);
            header(out, name, "counter");
            let _ = writeln!(out, "{key} {}", c.value());
        }
        for (name, read) in &inner.gauges {
            header(out, name, "gauge");
            let _ = writeln!(out, "{name} {}", format_float(read()));
        }
        for (key, h) in &inner.histograms {
            let (name, labels) = split_series(key);
            header(out, name, "histogram");
            let sep = if labels.is_empty() { "" } else { "," };
            let mut cumulative = 0u64;
            for (i, &bound) in LATENCY_BUCKETS.iter().enumerate() {
                cumulative = cumulative.wrapping_add(h.counts[i].load(Ordering::Relaxed));
                let bound = format_float(bound);
                let _ = writeln!(
                    out,
                    "{name}_bucket{{{labels}{sep}le=\"{bound}\"}} {cumulative}"
                );
            }
            cumulative =
                cumulative.wrapping_add(h.counts[LATENCY_BUCKETS.len()].load(Ordering::Relaxed));
            let _ = writeln!(
                out,
                "{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {cumulative}"
            );
            let braces = braces(labels);
            let sum = h.sum_nanos.load(Ordering::Relaxed) as f64 / 1e9;
            let _ = writeln!(out, "{name}_sum{braces} {}", format_float(sum));
            let _ = writeln!(out, "{name}_count{braces} {}", h.count());
        }
    }

    /// The whole exposition as a string.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }
}

fn series(name: &str, labels: &str) -> String {
    if labels.is_empty() {
        name.to_owned()
    } else {
        format!("{name}{{{labels}}}")
    }
}

fn split_series(key: &str) -> (&str, &str) {
    match key.split_once('{') {
        Some((name, rest)) => (name, rest.strip_suffix('}').unwrap_or(rest)),
        None => (key, ""),
    }
}

fn braces(labels: &str) -> String {
    if labels.is_empty() {
        String::new()
    } else {
        format!("{{{labels}}}")
    }
}

/// A float as steer wrote it: Go's `%.6f` with trailing zeros, and then a
/// trailing point, removed; infinities as `+Inf` and `-Inf`.
pub fn format_float(f: f64) -> String {
    if f.is_infinite() {
        return if f > 0.0 { "+Inf" } else { "-Inf" }.to_owned();
    }
    let s = format!("{f:.6}");
    if f.is_nan() {
        return s;
    }
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Labels rendered for [`Registry::counter`] and [`Registry::histogram`]:
/// `name="value"` pairs joined by commas, each value quoted as Go's `%q`
/// quotes it, as steer built them with `fmt.Sprintf`.
pub fn labels(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (name, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(name);
        out.push('=');
        push_go_quoted(&mut out, value);
    }
    out
}

/// `s` in double quotes the way Go's `strconv.Quote` writes it for the
/// characters a cluster name or an endpoint address can hold: `\\`, `\"`
/// and the C escapes, other ASCII control bytes as `\xNN`, C1 controls as
/// `\u00NN`, and every other character as itself.
fn push_go_quoted(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            c if c < ' ' || c == '\u{7f}' => {
                let _ = write!(out, "\\x{:02x}", u32::from(c));
            }
            c if ('\u{80}'..='\u{9f}').contains(&c) => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// How an upstream attempt ended, steer's `result` label on
/// `steer_upstream_attempts_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptResult {
    /// A response below 500, or 600 and up.
    Success,
    /// A 5xx response.
    Http5xx,
    /// The connection could not be made.
    ConnectError,
    /// No response head within the per-try or the overall timeout.
    Timeout,
    /// The connection broke before a response head.
    Reset,
}

impl AttemptResult {
    /// Every result, in the order steer registers their series.
    pub const ALL: [AttemptResult; 5] = [
        AttemptResult::Success,
        AttemptResult::Http5xx,
        AttemptResult::ConnectError,
        AttemptResult::Timeout,
        AttemptResult::Reset,
    ];

    /// The label value.
    pub fn as_str(self) -> &'static str {
        match self {
            AttemptResult::Success => "success",
            AttemptResult::Http5xx => "http_5xx",
            AttemptResult::ConnectError => "connect_error",
            AttemptResult::Timeout => "timeout",
            AttemptResult::Reset => "reset",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// How a TLS handshake on the proxy listener ended, steer's `result`
/// label on `steer_tls_handshakes_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeResult {
    /// The handshake completed.
    Ok,
    /// It did not finish within the handshake timeout.
    Timeout,
    /// A protocol error, no shared version or suite, or an unknown name
    /// under strict SNI.
    Error,
}

impl HandshakeResult {
    /// Every result, in the order steer registers their series.
    pub const ALL: [HandshakeResult; 3] = [
        HandshakeResult::Ok,
        HandshakeResult::Timeout,
        HandshakeResult::Error,
    ];

    /// The label value.
    pub fn as_str(self) -> &'static str {
        match self {
            HandshakeResult::Ok => "ok",
            HandshakeResult::Timeout => "timeout",
            HandshakeResult::Error => "error",
        }
    }
}

const RELOADS: &str = "steer_config_reloads_total";
const RELOADS_HELP: &str = "Configuration reloads.";
const NO_ROUTE: &str = "steer_no_route_total";
const NO_ROUTE_HELP: &str = "Requests that matched no route.";
const CONFIG_VERSION: &str = "steer_config_version";
const CONFIG_VERSION_HELP: &str = "Version of the configuration being served.";
const REQUESTS: &str = "steer_requests_total";
const REQUESTS_HELP: &str = "Requests by cluster and response status class.";
const DURATION: &str = "steer_request_duration_seconds";
const DURATION_HELP: &str = "Request latency as seen by the client, including retries.";
const RETRIES: &str = "steer_retries_total";
const RETRIES_HELP: &str = "Retry attempts sent.";
const BUDGET_EXHAUSTED: &str = "steer_retry_budget_exhausted_total";
const BUDGET_EXHAUSTED_HELP: &str = "Retries skipped because the retry budget was spent.";
const OVERFLOW: &str = "steer_overflow_total";
const OVERFLOW_HELP: &str = "Requests refused by the cluster concurrency limit.";
const ATTEMPTS: &str = "steer_upstream_attempts_total";
const ATTEMPTS_HELP: &str = "Upstream attempts by endpoint and result.";
const EJECTIONS: &str = "steer_outlier_ejections_total";
const EJECTIONS_HELP: &str = "Endpoints ejected by outlier detection.";
const TRANSITIONS: &str = "steer_health_transitions_total";
const TRANSITIONS_HELP: &str = "Active health check verdict changes.";
const HANDSHAKES: &str = "steer_tls_handshakes_total";
const HANDSHAKES_HELP: &str = "TLS handshakes on the proxy listener by result.";
const HANDSHAKE_DURATION: &str = "steer_tls_handshake_duration_seconds";
const HANDSHAKE_DURATION_HELP: &str = "Time to complete a TLS handshake on the proxy listener.";
const HANDSHAKES_IN_PROGRESS: &str = "steer_tls_handshakes_in_progress";
const HANDSHAKES_IN_PROGRESS_HELP: &str = "TLS handshakes running on the proxy listener.";

/// The status classes of `steer_requests_total`, by index: `unknown`, then
/// `1xx` to `5xx`.
const CODE_CLASSES: [&str; 6] = ["unknown", "1xx", "2xx", "3xx", "4xx", "5xx"];

/// steer's process-wide metric families, over a [`Registry`] that `/metrics`
/// renders.
#[derive(Debug)]
pub struct SteerMetrics {
    registry: Arc<Registry>,
    reload_success: Arc<Counter>,
    reload_failure: Arc<Counter>,
    no_route: Arc<Counter>,
}

impl SteerMetrics {
    /// Registers the families steer's proxy registers when it starts:
    /// reloads by result, requests without a route, and the configuration
    /// version, read from `config_version` on each scrape.
    pub fn new(
        registry: Arc<Registry>,
        config_version: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> SteerMetrics {
        let reload_success = registry.counter(RELOADS, r#"result="success""#, RELOADS_HELP);
        let reload_failure = registry.counter(RELOADS, r#"result="failure""#, RELOADS_HELP);
        let no_route = registry.counter(NO_ROUTE, "", NO_ROUTE_HELP);
        registry.gauge(CONFIG_VERSION, CONFIG_VERSION_HELP, move || {
            config_version() as f64
        });
        SteerMetrics {
            registry,
            reload_success,
            reload_failure,
            no_route,
        }
    }

    /// The registry the families live in.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Counts a reload that was applied.
    pub fn reload_succeeded(&self) {
        self.reload_success.inc();
    }

    /// Counts a reload that was refused.
    pub fn reload_failed(&self) {
        self.reload_failure.inc();
    }

    /// Counts a request that matched no route.
    pub fn no_route(&self) {
        self.no_route.inc();
    }

    /// The series of cluster `name` with its endpoints' addresses, in the
    /// configuration's order. Building a cluster with a name and endpoints
    /// seen before, as a reload does, gives the same series back.
    pub fn cluster<S: AsRef<str>>(&self, name: &str, endpoints: &[S]) -> ClusterMetrics {
        let r = &self.registry;
        let by_cluster = labels(&[("cluster", name)]);
        let requests = CODE_CLASSES.map(|class| {
            r.counter(
                REQUESTS,
                &labels(&[("cluster", name), ("code_class", class)]),
                REQUESTS_HELP,
            )
        });
        let attempts = endpoints
            .iter()
            .map(|ep| {
                AttemptResult::ALL.map(|result| {
                    r.counter(
                        ATTEMPTS,
                        &labels(&[
                            ("cluster", name),
                            ("endpoint", ep.as_ref()),
                            ("result", result.as_str()),
                        ]),
                        ATTEMPTS_HELP,
                    )
                })
            })
            .collect();
        ClusterMetrics {
            requests,
            latency: r.histogram(DURATION, &by_cluster, DURATION_HELP),
            retries: r.counter(RETRIES, &by_cluster, RETRIES_HELP),
            budget_exhausted: r.counter(BUDGET_EXHAUSTED, &by_cluster, BUDGET_EXHAUSTED_HELP),
            overflow: r.counter(OVERFLOW, &by_cluster, OVERFLOW_HELP),
            attempts,
        }
    }

    /// Counts an endpoint of `cluster` ejected by outlier detection for
    /// `reason`, such as `consecutive_gateway_failure`.
    pub fn outlier_ejection(&self, cluster: &str, reason: &str) {
        self.registry
            .counter(
                EJECTIONS,
                &labels(&[("cluster", cluster), ("reason", reason)]),
                EJECTIONS_HELP,
            )
            .inc();
    }

    /// Counts an active health check verdict of `cluster` changing to
    /// healthy or to unhealthy.
    pub fn health_transition(&self, cluster: &str, healthy: bool) {
        let to = if healthy { "healthy" } else { "unhealthy" };
        self.registry
            .counter(
                TRANSITIONS,
                &labels(&[("cluster", cluster), ("to", to)]),
                TRANSITIONS_HELP,
            )
            .inc();
    }

    /// Registers the TLS listener's families, which steer has only with
    /// `tls` configured; `in_progress` is read on each scrape.
    pub fn tls(&self, in_progress: impl Fn() -> u64 + Send + Sync + 'static) -> TlsMetrics {
        let r = &self.registry;
        let results = HandshakeResult::ALL.map(|result| {
            r.counter(
                HANDSHAKES,
                &labels(&[("result", result.as_str())]),
                HANDSHAKES_HELP,
            )
        });
        let duration = r.histogram(HANDSHAKE_DURATION, "", HANDSHAKE_DURATION_HELP);
        r.gauge(
            HANDSHAKES_IN_PROGRESS,
            HANDSHAKES_IN_PROGRESS_HELP,
            move || in_progress() as f64,
        );
        TlsMetrics { results, duration }
    }
}

/// One cluster's series, held by the request path.
#[derive(Debug, Clone)]
pub struct ClusterMetrics {
    requests: [Arc<Counter>; 6],
    latency: Arc<Histogram>,
    retries: Arc<Counter>,
    budget_exhausted: Arc<Counter>,
    overflow: Arc<Counter>,
    attempts: Vec<[Arc<Counter>; 5]>,
}

impl ClusterMetrics {
    /// Counts a finished request by the class of the status the client
    /// got, `unknown` outside 100 to 599, and records how long it took.
    pub fn request(&self, status: u16, took: Duration) {
        let class = if (100..600).contains(&status) {
            usize::from(status / 100)
        } else {
            0
        };
        self.requests[class].inc();
        self.latency.observe(took);
    }

    /// Counts a retry sent.
    pub fn retry(&self) {
        self.retries.inc();
    }

    /// Counts a retry skipped because the budget was spent.
    pub fn budget_exhausted(&self) {
        self.budget_exhausted.inc();
    }

    /// Counts a request refused by the cluster's `max_requests`.
    pub fn overflow(&self) {
        self.overflow.inc();
    }

    /// Counts an attempt on the endpoint at `endpoint`, its index in the
    /// configuration. An index past the endpoints is ignored.
    pub fn attempt(&self, endpoint: usize, result: AttemptResult) {
        if let Some(by_result) = self.attempts.get(endpoint) {
            by_result[result.index()].inc();
        }
    }

    /// The count of requests answered with a status of `class` (1 to 5),
    /// or of unknown class (0).
    pub fn requests(&self, class: usize) -> u64 {
        self.requests.get(class).map_or(0, |c| c.value())
    }

    /// Attempts on the endpoint at `endpoint` that ended with `result`.
    pub fn attempts(&self, endpoint: usize, result: AttemptResult) -> u64 {
        self.attempts
            .get(endpoint)
            .map_or(0, |by_result| by_result[result.index()].value())
    }
}

/// The TLS listener's series.
#[derive(Debug, Clone)]
pub struct TlsMetrics {
    results: [Arc<Counter>; 3],
    duration: Arc<Histogram>,
}

impl TlsMetrics {
    /// Counts a handshake by result; a completed one also records how long
    /// it took.
    pub fn handshake(&self, result: HandshakeResult, took: Duration) {
        self.results[result as usize].inc();
        if result == HandshakeResult::Ok {
            self.duration.observe(took);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// steer's `TestExposition`.
    #[test]
    fn exposition() {
        let r = Registry::new();
        let c = r.counter("reqs_total", r#"code="200""#, "Requests.");
        c.inc();
        c.add(2);
        r.counter("reqs_total", r#"code="500""#, "Requests.").inc();
        assert!(Arc::ptr_eq(
            &r.counter("reqs_total", r#"code="200""#, "Requests."),
            &c
        ));
        let h = r.histogram("lat_seconds", r#"route="/x""#, "Latency.");
        h.observe(Duration::from_micros(30));
        h.observe(Duration::from_millis(2));
        h.observe(Duration::from_secs(10));
        assert!(Arc::ptr_eq(
            &r.histogram("lat_seconds", r#"route="/x""#, "Latency."),
            &h
        ));
        assert_eq!(h.count(), 3);
        r.histogram("plain_seconds", "", "No labels.")
            .observe(Duration::from_millis(1));
        r.gauge("head", "Head.", || 12.0);
        let out = r.render();
        for want in [
            "# TYPE reqs_total counter",
            r#"reqs_total{code="200"} 3"#,
            r#"reqs_total{code="500"} 1"#,
            "# TYPE head gauge",
            "head 12",
            "# TYPE lat_seconds histogram",
            r#"lat_seconds_bucket{route="/x",le="0.00005"} 1"#,
            r#"lat_seconds_bucket{route="/x",le="0.0025"} 2"#,
            r#"lat_seconds_bucket{route="/x",le="+Inf"} 3"#,
            r#"lat_seconds_count{route="/x"} 3"#,
            r#"plain_seconds_bucket{le="0.001"} 1"#,
            "plain_seconds_count 1",
        ] {
            assert!(out.contains(want), "missing {want:?} in:\n{out}");
        }
        assert_eq!(out.matches("# TYPE reqs_total").count(), 1);
    }

    #[test]
    fn whole_output_in_steers_order() {
        let r = Registry::new();
        r.histogram("b_seconds", "", "B.")
            .observe(Duration::from_nanos(1_500_000_001));
        r.gauge("z_gauge", "Z.", || 0.5);
        r.gauge("a_gauge", "A.", || f64::NAN);
        r.counter("b_total", "", "B total.").add(7);
        r.counter("a_total", r#"x="2""#, "Old help.");
        // The newest series' help wins, as in steer.
        r.counter("a_total", r#"x="1""#, "A total.").inc();
        let out = r.render();
        let expected = "\
# HELP a_total A total.
# TYPE a_total counter
a_total{x=\"1\"} 1
a_total{x=\"2\"} 0
# HELP b_total B total.
# TYPE b_total counter
b_total 7
# HELP a_gauge A.
# TYPE a_gauge gauge
a_gauge NaN
# HELP z_gauge Z.
# TYPE z_gauge gauge
z_gauge 0.5
# HELP b_seconds B.
# TYPE b_seconds histogram
b_seconds_bucket{le=\"0.00005\"} 0
b_seconds_bucket{le=\"0.0001\"} 0
b_seconds_bucket{le=\"0.00025\"} 0
b_seconds_bucket{le=\"0.0005\"} 0
b_seconds_bucket{le=\"0.001\"} 0
b_seconds_bucket{le=\"0.0025\"} 0
b_seconds_bucket{le=\"0.005\"} 0
b_seconds_bucket{le=\"0.01\"} 0
b_seconds_bucket{le=\"0.025\"} 0
b_seconds_bucket{le=\"0.05\"} 0
b_seconds_bucket{le=\"0.1\"} 0
b_seconds_bucket{le=\"0.25\"} 0
b_seconds_bucket{le=\"1\"} 0
b_seconds_bucket{le=\"2.5\"} 1
b_seconds_bucket{le=\"+Inf\"} 1
b_seconds_sum 1.5
b_seconds_count 1
";
        assert_eq!(out, expected);
        assert!(format!("{r:?}").contains("counters: 3"));
    }

    #[test]
    fn bucket_bounds_are_inclusive() {
        let h = Histogram::default();
        h.observe(Duration::from_micros(50)); // on the first bound
        h.observe(Duration::from_nanos(50_001));
        h.observe(Duration::from_millis(2500)); // on the last bound
        h.observe(Duration::from_nanos(2_500_000_001));
        let counts: Vec<u64> = h.counts.iter().map(|c| c.load(Ordering::Relaxed)).collect();
        assert_eq!(counts[0], 1);
        assert_eq!(counts[1], 1);
        assert_eq!(counts[13], 1);
        assert_eq!(counts[14], 1);
    }

    #[test]
    fn huge_durations_count_as_gos_longest() {
        let h = Histogram::default();
        h.observe(Duration::MAX);
        h.observe(Duration::from_nanos(u64::MAX));
        assert_eq!(h.sum_nanos.load(Ordering::Relaxed), (i64::MAX as u64) * 2);
        assert_eq!(h.counts[LATENCY_BUCKETS.len()].load(Ordering::Relaxed), 2);
    }

    /// steer's `TestFormatFloat`, plus the cases Go's `%.6f` decides.
    #[test]
    fn floats_as_steer_writes_them() {
        for (f, want) in [
            (0.0, "0"),
            (1.0, "1"),
            (0.5, "0.5"),
            (0.00005, "0.00005"),
            (2.5, "2.5"),
            (100.0, "100"),
            (-0.25, "-0.25"),
            (1.0000004, "1"),
            (0.0000005, "0"), // 5e-7 is stored just below the tie
            (0.0000015, "0.000002"),
            (0.1234565, "0.123456"),
            (0.00000049, "0"),
            (123456.789, "123456.789"),
            (f64::INFINITY, "+Inf"),
            (f64::NEG_INFINITY, "-Inf"),
            (f64::NAN, "NaN"),
        ] {
            assert_eq!(format_float(f), want, "{f}");
        }
    }

    #[test]
    fn labels_are_quoted_like_go() {
        assert_eq!(
            labels(&[("cluster", "web"), ("endpoint", "[2001:db8::1]:80")]),
            r#"cluster="web",endpoint="[2001:db8::1]:80""#
        );
        assert_eq!(labels(&[]), "");
        // strconv.Quote's escapes.
        assert_eq!(
            labels(&[(
                "v",
                "a\"b\\c\u{7}\u{8}\u{c}\n\r\t\u{b}\u{0}\u{1f}\u{7f}\u{85}é"
            )]),
            r#"v="a\"b\\c\a\b\f\n\r\t\v\x00\x1f\x7f\u0085é""#
        );
    }

    #[test]
    fn split_series_round_trips() {
        assert_eq!(split_series("a"), ("a", ""));
        assert_eq!(split_series(r#"a{x="1"}"#), ("a", r#"x="1""#));
        assert_eq!(series("a", ""), "a");
        assert_eq!(series("a", r#"x="1""#), r#"a{x="1"}"#);
    }

    #[test]
    fn steer_families() {
        let version = Arc::new(AtomicU64::new(3));
        let v = Arc::clone(&version);
        let m = SteerMetrics::new(Arc::new(Registry::new()), move || v.load(Ordering::Relaxed));
        m.reload_succeeded();
        m.reload_failed();
        m.reload_failed();
        m.no_route();
        let web = m.cluster("web", &["10.0.0.1:80", "10.0.0.2:80"]);
        for (status, class) in [
            (99, 0),
            (100, 1),
            (204, 2),
            (308, 3),
            (499, 4),
            (599, 5),
            (600, 0),
        ] {
            let before = web.requests(class);
            web.request(status, Duration::from_millis(1));
            assert_eq!(web.requests(class), before + 1, "status {status}");
        }
        assert_eq!(web.requests(0), 2);
        assert_eq!(web.requests(6), 0);
        web.retry();
        web.budget_exhausted();
        web.overflow();
        for (i, result) in AttemptResult::ALL.into_iter().enumerate() {
            for _ in 0..=i {
                web.attempt(1, result);
            }
        }
        web.attempt(2, AttemptResult::Success); // no such endpoint
        assert_eq!(web.attempts(1, AttemptResult::Reset), 5);
        assert_eq!(web.attempts(0, AttemptResult::Reset), 0);
        assert_eq!(web.attempts(2, AttemptResult::Success), 0);
        // A reload builds the cluster again over the same series.
        let again = m.cluster("web", &["10.0.0.2:80".to_owned()]);
        assert_eq!(again.attempts(0, AttemptResult::Timeout), 4);
        m.outlier_ejection("web", "success_rate");
        m.health_transition("web", true);
        m.health_transition("web", false);
        m.health_transition("web", false);
        let tls = m.tls(|| 2);
        tls.handshake(HandshakeResult::Ok, Duration::from_millis(3));
        tls.handshake(HandshakeResult::Error, Duration::from_millis(3));
        tls.handshake(HandshakeResult::Timeout, Duration::from_secs(1));
        version.store(9, Ordering::Relaxed);

        let out = m.registry().render();
        for want in [
            r#"steer_config_reloads_total{result="failure"} 2"#,
            r#"steer_config_reloads_total{result="success"} 1"#,
            "steer_no_route_total 1",
            "steer_config_version 9",
            r#"steer_requests_total{cluster="web",code_class="unknown"} 2"#,
            r#"steer_requests_total{cluster="web",code_class="5xx"} 1"#,
            r#"steer_request_duration_seconds_count{cluster="web"} 7"#,
            r#"steer_retries_total{cluster="web"} 1"#,
            r#"steer_retry_budget_exhausted_total{cluster="web"} 1"#,
            r#"steer_overflow_total{cluster="web"} 1"#,
            r#"steer_upstream_attempts_total{cluster="web",endpoint="10.0.0.1:80",result="success"} 0"#,
            r#"steer_upstream_attempts_total{cluster="web",endpoint="10.0.0.2:80",result="http_5xx"} 2"#,
            r#"steer_upstream_attempts_total{cluster="web",endpoint="10.0.0.2:80",result="connect_error"} 3"#,
            r#"steer_outlier_ejections_total{cluster="web",reason="success_rate"} 1"#,
            r#"steer_health_transitions_total{cluster="web",to="healthy"} 1"#,
            r#"steer_health_transitions_total{cluster="web",to="unhealthy"} 2"#,
            r#"steer_tls_handshakes_total{result="ok"} 1"#,
            r#"steer_tls_handshakes_total{result="timeout"} 1"#,
            r#"steer_tls_handshakes_total{result="error"} 1"#,
            "steer_tls_handshake_duration_seconds_count 1",
            "steer_tls_handshakes_in_progress 2",
        ] {
            assert!(out.contains(want), "missing {want:?} in:\n{out}");
        }
        assert_eq!(AttemptResult::Reset.as_str(), "reset");
        assert_eq!(HandshakeResult::Ok.as_str(), "ok");
    }

    #[test]
    fn a_poisoned_lock_still_serves() {
        let r = Arc::new(Registry::new());
        r.counter("x_total", "", "X.").inc();
        let r2 = Arc::clone(&r);
        let _ = std::thread::spawn(move || {
            let _guard = r2.inner.lock().unwrap();
            panic!("poison the registry");
        })
        .join();
        assert!(r.inner.is_poisoned());
        r.counter("x_total", "", "X.").inc();
        assert!(r.render().contains("x_total 2"));
    }

    #[test]
    fn counters_count_across_threads() {
        let r = Arc::new(Registry::new());
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let r = Arc::clone(&r);
                std::thread::spawn(move || {
                    let c = r.counter("n_total", "", "N.");
                    let h = r.histogram("n_seconds", "", "N.");
                    for _ in 0..1000 {
                        c.inc();
                        h.observe(Duration::from_micros(1));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let out = r.render();
        assert!(out.contains("n_total 4000"));
        assert!(out.contains("n_seconds_count 4000"));
        assert!(out.contains("n_seconds_sum 0.004\n"));
    }
}
