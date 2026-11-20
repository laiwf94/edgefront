//! The result of one scenario against one target, as JSON, and the
//! Markdown report made from many.
//!
//! A [`BenchResult`] is what a run writes, one file per target:
//! `<out>/<scenario>/<target>.json`. Its fields are fixed by
//! `bench/schema/result.schema.json` (JSON Schema 2020-12), and every
//! result is checked against that file before it is written
//! ([`validate`]), so a result that does not match the schema is never
//! published. Latencies are in milliseconds and come from the load
//! generator's coordinated-omission-corrected response times; the
//! closed-loop service times sit beside them. `extra` holds a track's own
//! numbers (decisions per second, say) without a schema change.
//!
//! [`render`] turns a set of results into `docs/benchmarks/<run>.md`: one
//! section per track, one table per scenario, a row per target, with the
//! machine and the command that reproduces it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::histogram::Histogram;
use crate::load::{PhaseReport, Protocol};
use crate::measure::Resources;
use crate::scenario::Scenario;

/// The schema every result names, and the file that defines it.
pub const SCHEMA: &str = "edgefront-bench/result/v1";

/// The JSON Schema of [`BenchResult`], as checked in.
pub const SCHEMA_JSON: &str = include_str!("../../../bench/schema/result.schema.json");

/// One scenario run against one target.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchResult {
    /// Always [`SCHEMA`].
    pub schema: String,
    /// The run's name: the results of one invocation share it.
    pub run: String,
    /// Which track the comparison belongs to: `cache`, `proxy` or
    /// `limiter`.
    pub track: String,
    /// When the target started, RFC 3339 in UTC.
    pub started_at: String,
    /// The command that produced it.
    pub command: String,
    /// What was asked for.
    pub scenario: ScenarioInfo,
    /// What answered.
    pub target: TargetInfo,
    /// Where it ran.
    pub machine: Machine,
    /// Every phase but warm-up, merged.
    pub measured: Measured,
    /// Each phase of the schedule, warm-up included.
    pub phases: Vec<PhaseSummary>,
    /// What the target used.
    pub resources: Resources,
    /// A track's own numbers, by name.
    #[serde(default)]
    pub extra: BTreeMap<String, f64>,
}

/// The scenario, as run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioInfo {
    /// Its name.
    pub name: String,
    /// What it is for.
    pub description: String,
    /// `http1` or `http2`.
    pub protocol: String,
    /// Connections the load generator opened.
    pub connections: u64,
    /// The seed of its requests.
    pub seed: u64,
    /// Warm-up seconds.
    pub warmup_secs: f64,
    /// Measured seconds.
    pub measured_secs: f64,
    /// The mean scheduled rate over the measured phases.
    pub rate: f64,
    /// The hit ratio it was built for.
    pub hit_ratio_target: f64,
    /// The bytes its popular objects take.
    pub working_set_bytes: u64,
    /// The SNI the load went over TLS with; absent for cleartext.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_server_name: Option<String>,
}

impl ScenarioInfo {
    /// The description of `scenario`.
    pub fn of(scenario: &Scenario) -> ScenarioInfo {
        let schedule = scenario.schedule();
        let (warmup, measured): (Vec<_>, Vec<_>) =
            schedule.phases().iter().partition(|phase| phase.warmup);
        let secs = |phases: &[&crate::load::Phase]| {
            phases.iter().map(|p| p.duration.as_secs_f64()).sum::<f64>()
        };
        let measured_secs = secs(&measured);
        ScenarioInfo {
            name: scenario.name.clone(),
            description: scenario.description.clone(),
            protocol: protocol_name(scenario.protocol).to_owned(),
            connections: scenario.connections as u64,
            seed: scenario.seed,
            warmup_secs: secs(&warmup),
            measured_secs,
            rate: if measured_secs > 0.0 {
                scenario.measured_requests() as f64 / measured_secs
            } else {
                0.0
            },
            hit_ratio_target: scenario.keys.hit_ratio,
            working_set_bytes: scenario.working_set(),
            tls_server_name: scenario.tls.clone(),
        }
    }
}

/// `http1` or `http2`.
pub fn protocol_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Http1 => "http1",
        Protocol::Http2 => "http2",
    }
}

/// The target, as run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetInfo {
    /// Its short name, the directory under `bench/targets/`.
    pub name: String,
    /// The product it is.
    pub product: String,
    /// The product's version.
    pub version: String,
    /// `cache` or `proxy`: whether it is configured to cache.
    pub kind: String,
    /// The container images it ran, the main one first.
    pub images: Vec<Image>,
    /// The sum of their sizes.
    pub image_size_bytes: u64,
}

/// One container image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Image {
    /// What the target asked for: a pinned reference, or a local tag.
    pub reference: String,
    /// The image id the runtime resolved it to.
    pub id: String,
    /// Its size as the runtime reports it, uncompressed.
    pub size_bytes: u64,
}

/// The machine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Machine {
    /// A line a reader recognises, such as "16-core arm64, Docker 28".
    pub description: String,
    /// CPUs the container runtime has.
    pub cpus: u64,
    /// Memory the container runtime has, bytes.
    pub memory_bytes: u64,
    /// Its kernel version.
    pub kernel: String,
    /// Its architecture.
    pub arch: String,
    /// The container runtime and version.
    pub runtime: String,
    /// The target's CPU set.
    pub target_cpus: String,
    /// The origin's CPU set.
    pub origin_cpus: String,
    /// The load generator's CPU set.
    pub loadgen_cpus: String,
}

/// Latency quantiles, in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Latency {
    /// Median.
    pub p50_ms: f64,
    /// 90th percentile.
    pub p90_ms: f64,
    /// 99th percentile.
    pub p99_ms: f64,
    /// 99.9th percentile.
    pub p999_ms: f64,
    /// The slowest.
    pub max_ms: f64,
    /// The mean.
    pub mean_ms: f64,
}

impl Latency {
    /// The quantiles of a histogram of nanoseconds.
    pub fn of(histogram: &Histogram) -> Latency {
        let ms = |ns: u64| ns as f64 / 1e6;
        if histogram.is_empty() {
            return Latency::default();
        }
        Latency {
            p50_ms: ms(histogram.value_at_quantile(0.5)),
            p90_ms: ms(histogram.value_at_quantile(0.9)),
            p99_ms: ms(histogram.value_at_quantile(0.99)),
            p999_ms: ms(histogram.value_at_quantile(0.999)),
            max_ms: ms(histogram.max()),
            mean_ms: histogram.mean() / 1e6,
        }
    }
}

/// Failed requests by kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorCounts {
    /// Connect or handshake failures.
    pub connect: u64,
    /// No complete response within the timeout.
    pub timeout: u64,
    /// Failed while sending or reading.
    pub other: u64,
}

/// The measured phases.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measured {
    /// Requests the schedule sent.
    pub scheduled: u64,
    /// Requests answered in full.
    pub completed: u64,
    /// Requests that failed.
    pub errors: ErrorCounts,
    /// Completed responses by status class, 1xx to 5xx.
    pub status_classes: [u64; 5],
    /// Body bytes received.
    pub body_bytes: u64,
    /// Completed responses per second.
    pub throughput_rps: f64,
    /// Requests the origin answered during the measured phase.
    pub origin_requests: u64,
    /// `1 - origin_requests / completed`: the share answered without the
    /// origin.
    pub hit_ratio: f64,
    /// Response time from each request's due time.
    pub latency: Latency,
    /// Service time from each request's write.
    pub service_latency: Latency,
}

impl Measured {
    /// The measured phases of a run, with the origin's count over them.
    pub fn of(report: &PhaseReport, origin_requests: u64) -> Measured {
        let hit_ratio = if report.completed > 0 {
            (1.0 - origin_requests as f64 / report.completed as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        Measured {
            scheduled: report.scheduled,
            completed: report.completed,
            errors: ErrorCounts {
                connect: report.errors.connect,
                timeout: report.errors.timeout,
                other: report.errors.other,
            },
            status_classes: report.status_classes,
            body_bytes: report.body_bytes,
            throughput_rps: report.throughput(),
            origin_requests,
            hit_ratio,
            latency: Latency::of(&report.response_time),
            service_latency: Latency::of(&report.service_time),
        }
    }
}

/// One phase, in brief.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseSummary {
    /// The phase's name.
    pub name: String,
    /// Whether it was warm-up.
    pub warmup: bool,
    /// The scheduled rate.
    pub rate: f64,
    /// Its length, seconds.
    pub duration_secs: f64,
    /// Requests sent.
    pub scheduled: u64,
    /// Requests answered in full.
    pub completed: u64,
    /// Requests that failed.
    pub errors: u64,
    /// Completed responses per second.
    pub throughput_rps: f64,
    /// Response time quantiles.
    pub latency: Latency,
}

impl PhaseSummary {
    /// The summary of one phase.
    pub fn of(report: &PhaseReport) -> PhaseSummary {
        PhaseSummary {
            name: report.name.clone(),
            warmup: report.warmup,
            rate: report.rate,
            duration_secs: report.duration.as_secs_f64(),
            scheduled: report.scheduled,
            completed: report.completed,
            errors: report.errors.total(),
            throughput_rps: report.throughput(),
            latency: Latency::of(&report.response_time),
        }
    }
}

impl BenchResult {
    /// Reads a result and checks it against the schema.
    pub fn from_json(text: &str) -> Result<BenchResult, String> {
        let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let problems = validate(&value);
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
        serde_json::from_value(value).map_err(|e| e.to_string())
    }

    /// The result as pretty JSON, checked against the schema first.
    pub fn to_json(&self) -> Result<String, String> {
        let value = serde_json::to_value(self).map_err(|e| e.to_string())?;
        let problems = validate(&value);
        if !problems.is_empty() {
            return Err(format!(
                "the result breaks its schema: {}",
                problems.join("; ")
            ));
        }
        serde_json::to_string_pretty(&value)
            .map(|mut s| {
                s.push('\n');
                s
            })
            .map_err(|e| e.to_string())
    }
}

/// Reads every `*.json` result under `dir`, at any depth, sorted by path.
pub fn load_dir(dir: &Path) -> Result<Vec<BenchResult>, String> {
    let mut files = Vec::new();
    collect_json(dir, &mut files)?;
    files.sort();
    files
        .iter()
        .map(|path| {
            let text =
                std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
            BenchResult::from_json(&text).map_err(|e| format!("{}: {e}", path.display()))
        })
        .collect()
}

fn collect_json(dir: &Path, files: &mut Vec<std::path::PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.is_dir() {
            collect_json(&path, files)?;
        } else if path.extension().is_some_and(|e| e == "json") {
            files.push(path);
        }
    }
    Ok(())
}

/// Checks `value` against the result schema, returning every problem
/// with its JSON pointer.
pub fn validate(value: &Value) -> Vec<String> {
    match serde_json::from_str::<Value>(SCHEMA_JSON) {
        Ok(schema) => crate::schema::validate(&schema, value),
        Err(e) => vec![format!("the result schema does not parse: {e}")],
    }
}

/// The time as RFC 3339 in UTC, to the second.
pub fn rfc3339(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1 << 20) as f64)
}

fn ms(value: f64) -> String {
    if value >= 100.0 {
        format!("{value:.0} ms")
    } else if value >= 1.0 {
        format!("{value:.2} ms")
    } else {
        format!("{:.0} µs", value * 1000.0)
    }
}

/// Thousands separators: 12,345.
pub fn grouped(value: f64) -> String {
    let n = value.round() as u64;
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Collapses repeated runs (the same track, scenario and target) into one
/// result holding the median of each number, errors summed, with the count
/// in `extra["runs"]`. A result with no repeat is kept as it is.
pub fn merge_repeats(results: &[BenchResult]) -> Vec<BenchResult> {
    let mut groups: Vec<Vec<&BenchResult>> = Vec::new();
    for r in results {
        match groups.iter_mut().find(|g| {
            g[0].track == r.track
                && g[0].scenario.name == r.scenario.name
                && g[0].target.name == r.target.name
        }) {
            Some(g) => g.push(r),
            None => groups.push(vec![r]),
        }
    }
    groups
        .into_iter()
        .map(|g| {
            let mut out = g[0].clone();
            if g.len() == 1 {
                return out;
            }
            let med = |f: &dyn Fn(&BenchResult) -> f64| {
                let mut v: Vec<f64> = g.iter().map(|r| f(r)).collect();
                v.sort_by(f64::total_cmp);
                let n = v.len();
                if n % 2 == 1 {
                    v[n / 2]
                } else {
                    (v[n / 2 - 1] + v[n / 2]) / 2.0
                }
            };
            let m = &mut out.measured;
            m.throughput_rps = med(&|r| r.measured.throughput_rps);
            m.hit_ratio = med(&|r| r.measured.hit_ratio);
            m.latency.p50_ms = med(&|r| r.measured.latency.p50_ms);
            m.latency.p90_ms = med(&|r| r.measured.latency.p90_ms);
            m.latency.p99_ms = med(&|r| r.measured.latency.p99_ms);
            m.latency.p999_ms = med(&|r| r.measured.latency.p999_ms);
            m.errors = ErrorCounts {
                connect: g.iter().map(|r| r.measured.errors.connect).sum(),
                timeout: g.iter().map(|r| r.measured.errors.timeout).sum(),
                other: g.iter().map(|r| r.measured.errors.other).sum(),
            };
            out.resources.peak_rss_bytes = med(&|r| r.resources.peak_rss_bytes as f64) as u64;
            out.resources.peak_memory_bytes = med(&|r| r.resources.peak_memory_bytes as f64) as u64;
            out.resources.cpu_seconds = med(&|r| r.resources.cpu_seconds);
            out.resources.cpu_cores = med(&|r| r.resources.cpu_cores);
            out.extra.insert("runs".into(), g.len() as f64);
            out
        })
        .collect()
}

/// The Markdown report of `results`: a section per track (cache, proxy,
/// limiter, then any other, in that order), a table per scenario in the
/// order the scenarios first appear, a row per target.
pub fn render(title: &str, intro: &str, results: &[BenchResult]) -> String {
    let merged = merge_repeats(results);
    let results = merged.as_slice();
    let mut out = String::new();
    let _ = writeln!(out, "# {title}\n");
    if !intro.is_empty() {
        let _ = writeln!(out, "{}\n", intro.trim_end());
    }
    let mut machines: Vec<&Machine> = Vec::new();
    for result in results {
        if !machines.contains(&&result.machine) {
            machines.push(&result.machine);
        }
    }
    for machine in &machines {
        let _ = writeln!(
            out,
            "Machine: {}, with {} CPUs and {:.1} GiB for the container runtime ({}, kernel {}, {}). The target ran on CPUs {}, the origin on {} and the load generator on {}.\n",
            machine.description,
            machine.cpus,
            machine.memory_bytes as f64 / (1u64 << 30) as f64,
            machine.runtime,
            machine.kernel,
            machine.arch,
            machine.target_cpus,
            machine.origin_cpus,
            machine.loadgen_cpus
        );
    }
    let order = ["cache", "proxy", "limiter"];
    let mut tracks: Vec<&str> = Vec::new();
    for result in results {
        if !tracks.contains(&result.track.as_str()) {
            tracks.push(&result.track);
        }
    }
    tracks.sort_by_key(|t| order.iter().position(|o| o == t).unwrap_or(order.len()));
    for track in tracks {
        let mut name = track.to_owned();
        if let Some(first) = name.get_mut(0..1) {
            first.make_ascii_uppercase();
        }
        let _ = writeln!(out, "## {name}\n");
        let in_track: Vec<&BenchResult> = results.iter().filter(|r| r.track == track).collect();
        let mut scenarios: Vec<&str> = Vec::new();
        for result in &in_track {
            if !scenarios.contains(&result.scenario.name.as_str()) {
                scenarios.push(&result.scenario.name);
            }
        }
        for scenario in scenarios {
            let rows: Vec<&&BenchResult> = in_track
                .iter()
                .filter(|r| r.scenario.name == scenario)
                .collect();
            let info = &rows[0].scenario;
            let _ = writeln!(out, "### {scenario}\n");
            let _ = writeln!(
                out,
                "{}\n\nLoad: {} req/s over {} {} connections, {:.0} s measured after {:.0} s of warm-up. Hit ratio target {:.0}%, working set {}.\n",
                info.description,
                grouped(info.rate),
                info.connections,
                if info.protocol == "http2" {
                    "HTTP/2"
                } else {
                    "HTTP/1.1"
                },
                info.measured_secs,
                info.warmup_secs,
                info.hit_ratio_target * 100.0,
                mib(info.working_set_bytes)
            );
            let extras: Vec<&String> = {
                let mut keys: Vec<&String> = Vec::new();
                for row in &rows {
                    for key in row.extra.keys() {
                        if !keys.contains(&key) {
                            keys.push(key);
                        }
                    }
                }
                keys
            };
            let mut header = String::from(
                "| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image |",
            );
            let mut rule =
                String::from("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
            for key in &extras {
                let _ = write!(header, " {key} |");
                rule.push_str("---:|");
            }
            let _ = writeln!(out, "{header}\n{rule}");
            for row in rows {
                let m = &row.measured;
                let errors = m.errors.connect + m.errors.timeout + m.errors.other;
                let _ = write!(
                    out,
                    "| {} | {} | {} req/s | {} | {} | {} | {:.1}% | {} | {} | {} | {:.1} s ({:.2} cores) | {} |",
                    row.target.name,
                    row.target.version,
                    grouped(m.throughput_rps),
                    ms(m.latency.p50_ms),
                    ms(m.latency.p99_ms),
                    ms(m.latency.p999_ms),
                    m.hit_ratio * 100.0,
                    grouped(errors as f64),
                    mib(row.resources.peak_rss_bytes),
                    mib(row.resources.peak_memory_bytes),
                    row.resources.cpu_seconds,
                    row.resources.cpu_cores,
                    mib(row.target.image_size_bytes)
                );
                for key in &extras {
                    match row.extra.get(*key) {
                        Some(v) => {
                            let _ = write!(out, " {} |", grouped(*v));
                        }
                        None => out.push_str(" |"),
                    }
                }
                out.push('\n');
            }
            out.push('\n');
            let mut commands: Vec<&str> = Vec::new();
            for row in in_track.iter().filter(|r| r.scenario.name == scenario) {
                if !commands.contains(&row.command.as_str()) {
                    commands.push(&row.command);
                }
            }
            let _ = writeln!(
                out,
                "Reproduce with:\n\n```sh\n{}\n```\n",
                commands.join("\n")
            );
        }
    }
    out.push_str(
        "Latency is response time from each request's scheduled send time, so queueing behind a stall counts (no coordinated omission). Hit ratio is the share of measured requests the origin never saw. Peak RSS is the highest resident set (anon plus mapped file pages) summed over the target's containers, sampled every 100 ms; peak memory is the cgroup's `memory.peak`, page cache and tmpfs included. CPU is the target's CPU time over the measured phase. Image is the uncompressed size of the target's images.\n",
    );
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample(target: &str, track: &str, scenario: &str) -> BenchResult {
        BenchResult {
            schema: SCHEMA.into(),
            run: "2026-11-20-test".into(),
            track: track.into(),
            started_at: "2026-11-20T10:00:00Z".into(),
            command: "edgefront-bench run --scenario bench/scenarios/x.toml".into(),
            scenario: ScenarioInfo {
                name: scenario.into(),
                description: "A test.".into(),
                protocol: "http1".into(),
                connections: 8,
                seed: 1,
                warmup_secs: 1.0,
                measured_secs: 2.0,
                rate: 1000.0,
                hit_ratio_target: 0.9,
                working_set_bytes: 1 << 20,
                tls_server_name: None,
            },
            target: TargetInfo {
                name: target.into(),
                product: "Test".into(),
                version: "1.0".into(),
                kind: "cache".into(),
                images: vec![Image {
                    reference: "x@sha256:00".into(),
                    id: "sha256:11".into(),
                    size_bytes: 3 << 20,
                }],
                image_size_bytes: 3 << 20,
            },
            machine: Machine {
                description: "16-core arm64, Docker 28".into(),
                cpus: 16,
                memory_bytes: 8 << 30,
                kernel: "6.10".into(),
                arch: "aarch64".into(),
                runtime: "docker 28.0.4".into(),
                target_cpus: "0-3".into(),
                origin_cpus: "4-9".into(),
                loadgen_cpus: "10-15".into(),
            },
            measured: Measured {
                scheduled: 2000,
                completed: 2000,
                errors: ErrorCounts::default(),
                status_classes: [0, 2000, 0, 0, 0],
                body_bytes: 1000,
                throughput_rps: 1000.0,
                origin_requests: 200,
                hit_ratio: 0.9,
                latency: Latency {
                    p50_ms: 0.25,
                    p90_ms: 0.5,
                    p99_ms: 1.5,
                    p999_ms: 120.0,
                    max_ms: 130.0,
                    mean_ms: 0.3,
                },
                service_latency: Latency::default(),
            },
            phases: vec![],
            resources: Resources {
                peak_rss_bytes: 10 << 20,
                peak_memory_bytes: 20 << 20,
                cpu_seconds: 1.5,
                cpu_cores: 0.75,
                cpuset: "0-3".into(),
            },
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn a_result_round_trips_through_its_schema() {
        let result = sample("varnish", "cache", "cache-hit");
        let json = result.to_json().unwrap();
        assert_eq!(BenchResult::from_json(&json).unwrap(), result);
    }

    #[test]
    fn a_result_that_breaks_the_schema_is_refused() {
        let mut result = sample("varnish", "cache", "cache-hit");
        result.track = "other".into();
        assert!(result.to_json().unwrap_err().contains("/track"));
        let mut result = sample("varnish", "cache", "cache-hit");
        result.measured.hit_ratio = 1.5;
        assert!(
            result
                .to_json()
                .unwrap_err()
                .contains("/measured/hit_ratio")
        );
        let json = sample("a", "cache", "s").to_json().unwrap();
        let broken = json.replacen(
            "\"schema\": \"edgefront-bench/result/v1\"",
            "\"schema\": \"v0\"",
            1,
        );
        assert!(
            BenchResult::from_json(&broken)
                .unwrap_err()
                .contains("/schema")
        );
        assert!(BenchResult::from_json("{").is_err());
    }

    #[test]
    fn renders_a_table_per_scenario_and_a_section_per_track() {
        let mut limiter = sample("pacerd", "limiter", "decisions");
        limiter.extra.insert("decisions/s".into(), 1_250_000.0);
        let results = vec![
            limiter,
            sample("edgefront", "cache", "cache-hit"),
            sample("varnish", "cache", "cache-hit"),
            sample("edgefront", "cache", "miss-heavy"),
            sample("steer", "proxy", "proxy"),
        ];
        let text = render("Baseline", "Intro line.", &results);
        let cache = text.find("## Cache").unwrap();
        let proxy = text.find("## Proxy").unwrap();
        let limiter = text.find("## Limiter").unwrap();
        assert!(cache < proxy && proxy < limiter, "{text}");
        assert!(text.starts_with("# Baseline\n\nIntro line.\n"));
        assert_eq!(text.matches("Machine: 16-core arm64, Docker 28").count(), 1);
        let hit = text.find("### cache-hit").unwrap();
        let miss = text.find("### miss-heavy").unwrap();
        assert!(hit < miss);
        assert!(text.contains("| varnish | 1.0 | 1,000 req/s | 250 µs | 1.50 ms | 120 ms | 90.0% | 0 | 10.0 MiB | 20.0 MiB | 1.5 s (0.75 cores) | 3.0 MiB |"), "{text}");
        assert!(text.contains("| decisions/s |"));
        assert!(text.contains(" 1,250,000 |"));
        assert!(text.contains("over 8 HTTP/1.1 connections"), "{text}");
        assert!(text.contains("```sh\nedgefront-bench run"));
    }

    #[test]
    fn repeats_collapse_to_medians() {
        let mut runs = Vec::new();
        for (p99, errs) in [(3.0, 1), (1.0, 0), (2.0, 2)] {
            let mut r = sample("steer", "proxy", "p");
            r.measured.latency.p99_ms = p99;
            r.measured.errors.timeout = errs;
            runs.push(r);
        }
        let mut even = sample("edgefront", "proxy", "p");
        even.resources.cpu_seconds = 1.0;
        let mut even2 = even.clone();
        even2.resources.cpu_seconds = 2.0;
        runs.push(even);
        runs.push(even2);
        runs.push(sample("solo", "cache", "c"));
        let merged = merge_repeats(&runs);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].measured.latency.p99_ms, 2.0);
        assert_eq!(merged[0].measured.errors.timeout, 3);
        assert_eq!(merged[0].extra["runs"], 3.0);
        assert_eq!(merged[1].resources.cpu_seconds, 1.5);
        assert!(merged[2].extra.is_empty());
        assert!(render("t", "", &runs).contains("| runs |"));
    }

    #[test]
    fn writes_and_reads_a_directory_of_results() {
        let dir =
            std::env::temp_dir().join(format!("edgefront-bench-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cache-hit")).unwrap();
        for name in ["varnish", "edgefront"] {
            std::fs::write(
                dir.join("cache-hit").join(format!("{name}.json")),
                sample(name, "cache", "cache-hit").to_json().unwrap(),
            )
            .unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let results = load_dir(&dir).unwrap();
        assert_eq!(
            results
                .iter()
                .map(|r| r.target.name.as_str())
                .collect::<Vec<_>>(),
            ["edgefront", "varnish"]
        );
        std::fs::write(dir.join("bad.json"), "{}").unwrap();
        assert!(load_dir(&dir).unwrap_err().contains("bad.json"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(load_dir(&dir).is_err());
    }

    #[test]
    fn formats_numbers_and_times() {
        assert_eq!(grouped(0.0), "0");
        assert_eq!(grouped(999.4), "999");
        assert_eq!(grouped(1234567.0), "1,234,567");
        assert_eq!(ms(0.0123), "12 µs");
        assert_eq!(ms(12.345), "12.35 ms");
        assert_eq!(ms(1234.0), "1234 ms");
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_795_000_000)),
            "2026-11-18T11:06:40Z"
        );
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00Z"
        );
    }

    #[test]
    fn latency_of_an_empty_histogram_is_zero() {
        assert_eq!(Latency::of(&Histogram::default()), Latency::default());
        let mut h = Histogram::default();
        h.record(2_000_000);
        let l = Latency::of(&h);
        assert!((l.p99_ms - 2.0).abs() < 0.01 && (l.mean_ms - 2.0).abs() < 0.01);
    }
}
