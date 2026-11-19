//! The benchmark regression gate.
//!
//! A pull request runs the gate scenario `runs` times (5) and compares
//! each metric's samples with the baseline's: the same scenario run on the
//! commit named by `baseline_commit`. On a CPU model listed in
//! `committed_models` those are the samples committed in
//! `bench/baseline/gate.json`; on any other model (today, every model,
//! because the runners' noise ruled the committed samples out) CI builds
//! the baseline commit and alternates its runs with the candidate's on the
//! same runner, and records them with [`Baseline::record_target`]. A
//! metric regresses when both hold:
//!
//! - **It is significant.** A one-sided Mann-Whitney U test says the
//!   candidate's samples are worse than the baseline's at `p < p_value`
//!   (0.01). With 5 samples a side the exact test reaches that only when
//!   every candidate sample is worse than every baseline sample but at
//!   most one pair: one noisy run cannot fail a pull request.
//! - **It is large.** The candidate's median is worse than the baseline's
//!   by more than the metric's `tolerance`, 5% (`bench/gates.toml` records
//!   the noise measured on the CI runners that set it).
//!
//! Size has fixed ceilings instead, since it does not vary between runs:
//! the binary's bytes and the image's.
//!
//! The verdict is JSON ([`Verdict`]) naming every metric with its medians,
//! change, p-value and tolerance, and the failing ones, and
//! [`Verdict::markdown`] is the job summary.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::report::BenchResult;

/// The schema name of a baseline file.
pub const BASELINE_SCHEMA: &str = "edgefront-bench/baseline/v1";

/// The schema name of a verdict.
pub const VERDICT_SCHEMA: &str = "edgefront-bench/verdict/v1";

/// Which way is better.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Better {
    /// Smaller is better (latency).
    Lower,
    /// Larger is better (throughput).
    Higher,
}

/// How one metric is judged.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricRule {
    /// Which way is better.
    pub better: Better,
    /// The smallest relative change of the median that counts, such as
    /// 0.05.
    pub tolerance: f64,
}

/// Fixed ceilings, in bytes.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ceilings {
    /// The release binary.
    pub binary_bytes: Option<u64>,
    /// The target's container image, uncompressed.
    pub image_bytes: Option<u64>,
}

/// `bench/gates.toml`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateConfig {
    /// The scenario the gate runs.
    pub scenario: String,
    /// The target it runs against.
    pub target: String,
    /// CPUs the target gets.
    pub target_cpus: usize,
    /// Runs per side.
    pub runs: usize,
    /// The significance level.
    pub p_value: f64,
    /// The committed baseline.
    pub baseline: String,
    /// The commit the baseline was recorded from, which the gate builds and
    /// runs beside a candidate on a CPU model outside `committed_models`.
    pub baseline_commit: String,
    /// The metrics, by name (see [`metric`]).
    pub metrics: BTreeMap<String, MetricRule>,
    /// Size ceilings.
    #[serde(default)]
    pub ceilings: Ceilings,
    /// The CPU models whose committed baseline samples a pull request is
    /// judged against directly: those whose measured run-to-run noise is
    /// within the tolerances. On any other model the gate runs the
    /// baseline's commit on the same runner, interleaved with the
    /// candidate, and judges against that ([`GateConfig::plan`]).
    #[serde(default)]
    pub committed_models: Vec<String>,
}

impl GateConfig {
    /// Reads and checks a gate config.
    pub fn from_toml(text: &str) -> Result<GateConfig, String> {
        let config: GateConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        if config.runs < 3 {
            return Err(format!(
                "runs = {} is too few for a rank test; use 3 or more",
                config.runs
            ));
        }
        if !(config.p_value > 0.0 && config.p_value < 1.0) {
            return Err(format!(
                "p_value = {} must be between 0 and 1",
                config.p_value
            ));
        }
        if config.target_cpus == 0 {
            return Err("target_cpus must be above 0".into());
        }
        if config.baseline_commit.len() != 40
            || !config
                .baseline_commit
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        {
            return Err(format!(
                "baseline_commit {:?} must be a full 40-digit commit",
                config.baseline_commit
            ));
        }
        if config.metrics.is_empty() {
            return Err("no [metrics]".into());
        }
        for (name, rule) in &config.metrics {
            if metric_of(name).is_none() {
                return Err(format!(
                    "unknown metric {name:?}; known: {}",
                    METRICS.join(", ")
                ));
            }
            if !(rule.tolerance >= 0.0 && rule.tolerance < 1.0) {
                return Err(format!(
                    "{name}: tolerance {} must be in [0, 1)",
                    rule.tolerance
                ));
            }
        }
        // The smallest exact one-sided p-value with `runs` a side is
        // 1 / C(2 runs, runs); a level below it could never fail.
        let floor = 1.0 / binomial(2 * config.runs, config.runs);
        if config.p_value <= floor {
            return Err(format!(
                "with {} runs a side the smallest p-value is {floor:.5}, so p_value = {} can never be reached",
                config.runs, config.p_value
            ));
        }
        Ok(config)
    }

    /// How a pull request on `machine` is judged: `committed` against the
    /// committed samples, or `measure` against the baseline commit run on
    /// the same runner.
    pub fn plan(&self, machine: &str) -> &'static str {
        if self.committed_models.iter().any(|m| m == machine.trim()) {
            "committed"
        } else {
            "measure"
        }
    }

    /// The plan line `edgefront-bench gate --plan` prints: the plan, then
    /// the baseline commit.
    pub fn plan_line(&self, machine: &str) -> String {
        format!("{} {}", self.plan(machine), self.baseline_commit)
    }

    /// Reads `path`.
    pub fn load(path: &Path) -> Result<GateConfig, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        GateConfig::from_toml(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// The metrics a gate can judge.
pub const METRICS: &[&str] = &[
    "p50_ms",
    "p99_ms",
    "p999_ms",
    "throughput_rps",
    "requests_per_cpu_second",
];

type Extract = fn(&BenchResult) -> Option<f64>;

fn metric_of(name: &str) -> Option<Extract> {
    let f: Extract = match name {
        "p50_ms" => |r| Some(r.measured.latency.p50_ms),
        "p99_ms" => |r| Some(r.measured.latency.p99_ms),
        "p999_ms" => |r| Some(r.measured.latency.p999_ms),
        "throughput_rps" => |r| Some(r.measured.throughput_rps),
        // Completed requests per second of the target's CPU time: the
        // throughput a CPU buys. An open-loop run's own throughput is its
        // schedule, so this is the throughput that moves when the request
        // path gets slower.
        "requests_per_cpu_second" => |r| {
            (r.resources.cpu_seconds > 0.0)
                .then(|| r.measured.completed as f64 / r.resources.cpu_seconds)
        },
        _ => return None,
    };
    Some(f)
}

/// The value of metric `name` in `result`, if it has one.
pub fn metric(result: &BenchResult, name: &str) -> Option<f64> {
    metric_of(name)?(result)
}

/// One machine's baseline: each metric's samples from gate runs on
/// `main`, pooled over several runners of that machine type.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineBaseline {
    /// The commit the samples were recorded from.
    pub commit: String,
    /// When the first of them was recorded, RFC 3339.
    pub recorded_at: String,
    /// Samples by metric.
    pub samples: BTreeMap<String, Vec<f64>>,
}

/// The committed baseline: one [`MachineBaseline`] per machine type, keyed
/// by the CPU's model name. GitHub's hosted runners come on more than one
/// CPU model, and the same build measures up to 60% apart between them, so
/// a pull request is compared only with runs on its own model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    /// Always [`BASELINE_SCHEMA`].
    pub schema: String,
    /// The gate scenario's name.
    pub scenario: String,
    /// The target.
    pub target: String,
    /// The baselines by machine.
    pub machines: BTreeMap<String, MachineBaseline>,
}

impl Baseline {
    /// An empty baseline for `config`.
    pub fn new(config: &GateConfig) -> Baseline {
        Baseline {
            schema: BASELINE_SCHEMA.into(),
            scenario: config.scenario_name(),
            target: config.target.clone(),
            machines: BTreeMap::new(),
        }
    }

    /// Sets `machine`'s samples to every metric in `config` from
    /// `results`, replacing what it had.
    pub fn record(
        &mut self,
        config: &GateConfig,
        machine: &str,
        results: &[BenchResult],
        commit: &str,
    ) -> Result<(), String> {
        self.record_target(config, &config.target, machine, results, commit)
    }

    /// As [`Baseline::record`], from the results of another target name:
    /// the baseline commit's build, run beside the candidate under a name
    /// of its own.
    pub fn record_target(
        &mut self,
        config: &GateConfig,
        target: &str,
        machine: &str,
        results: &[BenchResult],
        commit: &str,
    ) -> Result<(), String> {
        if machine.trim().is_empty() {
            return Err("the machine key is empty".into());
        }
        let results = select(config, target, results)?;
        let mut samples = BTreeMap::new();
        for name in config.metrics.keys() {
            samples.insert(name.clone(), values(&results, name)?);
        }
        let recorded_at = results
            .iter()
            .map(|r| r.started_at.clone())
            .min()
            .unwrap_or_default();
        self.machines.insert(
            machine.trim().to_owned(),
            MachineBaseline {
                commit: commit.into(),
                recorded_at,
                samples,
            },
        );
        Ok(())
    }

    /// Reads a baseline file.
    pub fn load(path: &Path) -> Result<Baseline, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let baseline: Baseline =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if baseline.schema != BASELINE_SCHEMA {
            return Err(format!(
                "{}: schema {:?} is not {BASELINE_SCHEMA}",
                path.display(),
                baseline.schema
            ));
        }
        Ok(baseline)
    }
}

impl GateConfig {
    /// The scenario's name: its file name without `.toml`.
    pub fn scenario_name(&self) -> String {
        Path::new(&self.scenario)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&self.scenario)
            .to_owned()
    }
}

/// The gate's results: the configured scenario against the configured
/// target, at least `runs` of them.
fn select<'a>(
    config: &GateConfig,
    target: &str,
    results: &'a [BenchResult],
) -> Result<Vec<&'a BenchResult>, String> {
    let scenario = config.scenario_name();
    let chosen: Vec<&BenchResult> = results
        .iter()
        .filter(|r| r.scenario.name == scenario && r.target.name == target)
        .collect();
    if chosen.len() < config.runs {
        return Err(format!(
            "{} results of {scenario} against {target}, and the gate needs {}",
            chosen.len(),
            config.runs
        ));
    }
    for result in &chosen {
        let errors = result.measured.errors.connect
            + result.measured.errors.timeout
            + result.measured.errors.other;
        if errors > 0 || result.measured.completed < result.measured.scheduled {
            return Err(format!(
                "a gate run of {} had {errors} errors and answered {} of {} requests; a run that fails requests is not a measurement",
                result.target.name, result.measured.completed, result.measured.scheduled
            ));
        }
    }
    Ok(chosen)
}

fn values(results: &[&BenchResult], name: &str) -> Result<Vec<f64>, String> {
    results
        .iter()
        .map(|r| {
            metric(r, name).ok_or_else(|| format!("a result has no {name} (no CPU time measured?)"))
        })
        .collect()
}

/// One metric's judgement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricVerdict {
    /// Its name.
    pub name: String,
    /// Which way is better.
    pub better: Better,
    /// The baseline's median.
    pub baseline_median: f64,
    /// The candidate's median.
    pub candidate_median: f64,
    /// The relative change of the median, positive when worse.
    pub worse_by: f64,
    /// The tolerance it is held to.
    pub tolerance: f64,
    /// The one-sided Mann-Whitney p-value that the candidate is worse.
    pub p_value: f64,
    /// Both significant and over the tolerance.
    pub regressed: bool,
}

/// One ceiling's judgement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CeilingVerdict {
    /// What is measured.
    pub name: String,
    /// Its size.
    pub bytes: u64,
    /// The ceiling.
    pub ceiling: u64,
    /// Over the ceiling.
    pub over: bool,
}

/// The gate's answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    /// Always [`VERDICT_SCHEMA`].
    pub schema: String,
    /// Whether the change passes.
    pub pass: bool,
    /// The names of the failing metrics and ceilings.
    pub failing: Vec<String>,
    /// The significance level.
    pub p_value: f64,
    /// Runs per side.
    pub runs: usize,
    /// The machine the candidate ran on.
    pub machine: String,
    /// Whether the baseline had that machine, so the metrics were judged.
    /// When it did not, only the size ceilings were, and the summary says
    /// so.
    pub judged: bool,
    /// The baseline's commit for that machine, or empty.
    pub baseline_commit: String,
    /// Each metric.
    pub metrics: Vec<MetricVerdict>,
    /// Each ceiling.
    pub ceilings: Vec<CeilingVerdict>,
}

/// Judges `candidates`, run on `machine`, against `baseline`.
/// `binary_bytes` is the release binary's size, when there is a binary
/// ceiling.
pub fn evaluate(
    config: &GateConfig,
    baseline: &Baseline,
    machine: &str,
    candidates: &[BenchResult],
    binary_bytes: Option<u64>,
) -> Result<Verdict, String> {
    let chosen = select(config, &config.target, candidates)?;
    let mut metrics = Vec::new();
    let mut failing = Vec::new();
    let own = baseline.machines.get(machine.trim());
    for (name, rule) in &config.metrics {
        let Some(own) = own else { break };
        let base = own
            .samples
            .get(name)
            .filter(|s| s.len() >= 3)
            .ok_or_else(|| {
                format!("the baseline has fewer than 3 samples of {name}; record it again")
            })?;
        let cand = values(&chosen, name)?;
        let (b, c) = (median(base), median(&cand));
        let worse_by = match rule.better {
            Better::Lower => (c - b) / b,
            Better::Higher => (b - c) / b,
        };
        let p_value = match rule.better {
            Better::Lower => mann_whitney_greater(&cand, base),
            Better::Higher => mann_whitney_greater(base, &cand),
        };
        let regressed = p_value < config.p_value && worse_by > rule.tolerance;
        if regressed {
            failing.push(name.clone());
        }
        metrics.push(MetricVerdict {
            name: name.clone(),
            better: rule.better,
            baseline_median: b,
            candidate_median: c,
            worse_by,
            tolerance: rule.tolerance,
            p_value,
            regressed,
        });
    }
    let mut ceilings = Vec::new();
    if let Some(ceiling) = config.ceilings.binary_bytes {
        let bytes = binary_bytes
            .ok_or("bench/gates.toml sets a binary ceiling, so the gate needs --binary")?;
        ceilings.push(CeilingVerdict {
            name: "binary_bytes".into(),
            bytes,
            ceiling,
            over: bytes > ceiling,
        });
    }
    if let Some(ceiling) = config.ceilings.image_bytes {
        let bytes = chosen
            .iter()
            .map(|r| r.target.image_size_bytes)
            .max()
            .unwrap_or(0);
        ceilings.push(CeilingVerdict {
            name: "image_bytes".into(),
            bytes,
            ceiling,
            over: bytes > ceiling,
        });
    }
    failing.extend(ceilings.iter().filter(|c| c.over).map(|c| c.name.clone()));
    Ok(Verdict {
        schema: VERDICT_SCHEMA.into(),
        pass: failing.is_empty(),
        failing,
        p_value: config.p_value,
        runs: config.runs,
        machine: machine.trim().to_owned(),
        judged: own.is_some(),
        baseline_commit: own.map(|o| o.commit.clone()).unwrap_or_default(),
        metrics,
        ceilings,
    })
}

impl Verdict {
    /// The verdict as a Markdown job summary.
    pub fn markdown(&self) -> String {
        let mut out = format!(
            "## Benchmark gate: {}\n\n{} runs against the baseline from `{}` on this CPU model; a metric fails when a one-sided Mann-Whitney U test gives p < {} and its median is worse by more than its tolerance.\n\n| Metric | Baseline median | This change | Worse by | Tolerance | p | Verdict |\n|---|---:|---:|---:|---:|---:|---|\n",
            if self.pass { "pass" } else { "FAIL" },
            self.runs,
            self.baseline_commit,
            self.p_value
        );
        if !self.judged {
            out = format!(
                "## Benchmark gate: {}\n\nThe baseline has no runs on this runner's CPU ({}), and runs on different CPU models are not comparable, so only the size ceilings were judged. Record a baseline for it with the bench workflow's `record` input.\n",
                if self.pass {
                    "sizes pass, speed not judged"
                } else {
                    "FAIL"
                },
                self.machine
            );
        }
        for m in &self.metrics {
            out.push_str(&format!(
                "| {} | {:.4} | {:.4} | {:+.1}% | {:.1}% | {:.4} | {} |\n",
                m.name,
                m.baseline_median,
                m.candidate_median,
                m.worse_by * 100.0,
                m.tolerance * 100.0,
                m.p_value,
                if m.regressed { "regressed" } else { "ok" }
            ));
        }
        if !self.ceilings.is_empty() {
            out.push_str("\n| Size | Bytes | Ceiling | Verdict |\n|---|---:|---:|---|\n");
            for c in &self.ceilings {
                out.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    c.name,
                    crate::report::grouped(c.bytes as f64),
                    crate::report::grouped(c.ceiling as f64),
                    if c.over { "over" } else { "ok" }
                ));
            }
        }
        out
    }
}

/// The median.
pub fn median(values: &[f64]) -> f64 {
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n == 0 {
        return f64::NAN;
    }
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

fn binomial(n: usize, k: usize) -> f64 {
    (0..k).fold(1.0, |acc, i| acc * (n - i) as f64 / (i + 1) as f64)
}

/// The one-sided Mann-Whitney U test's p-value for the hypothesis that
/// `a` tends to be greater than `b`: the chance, if both came from one
/// distribution, of a rank sum for `a` at least as large as the one seen.
///
/// Ties get midranks. While there are at most 250,000 ways to choose
/// `a`'s ranks from the pooled ranks (5 against up to 25, 10 against 10),
/// the p-value is exact: every way is counted. Beyond that it is the
/// normal approximation with the tie correction and a continuity
/// correction.
pub fn mann_whitney_greater(a: &[f64], b: &[f64]) -> f64 {
    let (n, m) = (a.len(), b.len());
    if n == 0 || m == 0 {
        return 1.0;
    }
    let mut pooled: Vec<(f64, bool)> = a
        .iter()
        .map(|&x| (x, true))
        .chain(b.iter().map(|&x| (x, false)))
        .collect();
    pooled.sort_by(|x, y| x.0.total_cmp(&y.0));
    let total = n + m;
    let mut ranks = vec![0.0; total];
    let mut i = 0;
    while i < total {
        let mut j = i;
        while j + 1 < total && pooled[j + 1].0 == pooled[i].0 {
            j += 1;
        }
        let rank = (i + j) as f64 / 2.0 + 1.0;
        for r in ranks.iter_mut().take(j + 1).skip(i) {
            *r = rank;
        }
        i = j + 1;
    }
    let observed: f64 = pooled
        .iter()
        .zip(&ranks)
        .filter(|(p, _)| p.1)
        .map(|(_, r)| r)
        .sum();
    if binomial(total, n.min(m)) <= 250_000.0 {
        // Count the subsets of size n whose rank sum reaches the observed.
        let mut at_least = 0u64;
        let mut all = 0u64;
        count_subsets(&ranks, n, 0, 0.0, observed - 1e-9, &mut at_least, &mut all);
        return at_least as f64 / all as f64;
    }
    let (nf, mf) = (n as f64, m as f64);
    let u = observed - nf * (nf + 1.0) / 2.0;
    let mean = nf * mf / 2.0;
    let mut ties = 0.0;
    let mut k = 0;
    while k < total {
        let mut j = k;
        while j + 1 < total && pooled[j + 1].0 == pooled[k].0 {
            j += 1;
        }
        let t = (j - k + 1) as f64;
        ties += t * t * t - t;
        k = j + 1;
    }
    let nt = total as f64;
    let variance = nf * mf / 12.0 * ((nt + 1.0) - ties / (nt * (nt - 1.0)));
    if variance <= 0.0 {
        return 1.0;
    }
    let z = (u - mean - 0.5) / variance.sqrt();
    0.5 * erfc(z / std::f64::consts::SQRT_2)
}

fn count_subsets(
    ranks: &[f64],
    left: usize,
    from: usize,
    sum: f64,
    threshold: f64,
    at_least: &mut u64,
    all: &mut u64,
) {
    if left == 0 {
        *all += 1;
        if sum >= threshold {
            *at_least += 1;
        }
        return;
    }
    for i in from..=ranks.len() - left {
        count_subsets(
            ranks,
            left - 1,
            i + 1,
            sum + ranks[i],
            threshold,
            at_least,
            all,
        );
    }
}

/// The complementary error function, to about 1e-7 (Numerical Recipes'
/// Chebyshev fit).
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let r = t
        * (-z * z - 1.265_512_23
            + t * (1.000_023_68
                + t * (0.374_091_96
                    + t * (0.096_784_18
                        + t * (-0.186_288_06
                            + t * (0.278_868_07
                                + t * (-1.135_203_98
                                    + t * (1.488_515_87
                                        + t * (-0.822_152_23 + t * 0.170_872_77)))))))))
            .exp();
    if x >= 0.0 { r } else { 2.0 - r }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::tests::sample;

    const CONFIG: &str = r#"
        scenario = "bench/ci/gate.toml"
        target = "edgefront"
        target_cpus = 1
        runs = 5
        p_value = 0.01
        baseline = "bench/baseline/gate.json"
        baseline_commit = "0f4230eb560a25a1e5e06d27b6a9d206d949de1b"
        [metrics.p99_ms]
        better = "lower"
        tolerance = 0.05
        [metrics.requests_per_cpu_second]
        better = "higher"
        tolerance = 0.05
        [ceilings]
        binary_bytes = 10000000
        image_bytes = 20000000
    "#;

    fn result(p99: f64, per_cpu: f64) -> BenchResult {
        let mut r = sample("edgefront", "cache", "gate");
        r.measured.latency.p99_ms = p99;
        r.resources.cpu_seconds = r.measured.completed as f64 / per_cpu;
        r.target.image_size_bytes = 15_000_000;
        r
    }

    fn runs(p99: &[f64], per_cpu: &[f64]) -> Vec<BenchResult> {
        p99.iter()
            .zip(per_cpu)
            .map(|(&a, &b)| result(a, b))
            .collect()
    }

    const CPU: &str = "AMD EPYC 9V74 80-Core Processor";

    fn baseline(config: &GateConfig) -> Baseline {
        let mut baseline = Baseline::new(config);
        baseline
            .record(
                config,
                CPU,
                &runs(
                    &[1.00, 1.02, 0.98, 1.01, 0.99],
                    &[10_000.0, 10_100.0, 9_900.0, 10_050.0, 9_950.0],
                ),
                "abc1234",
            )
            .unwrap();
        baseline
    }

    #[test]
    fn exact_p_values_match_the_tables() {
        // Complete separation, 5 against 5: 1 / C(10, 5).
        let p = mann_whitney_greater(&[6.0, 7.0, 8.0, 9.0, 10.0], &[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert!((p - 1.0 / 252.0).abs() < 1e-12, "{p}");
        // The other way round: certain.
        assert_eq!(
            mann_whitney_greater(&[1.0, 2.0, 3.0, 4.0, 5.0], &[6.0, 7.0, 8.0, 9.0, 10.0]),
            1.0
        );
        // One pair out of order: U = 24 of 25, P(U >= 24) = 2 / 252.
        let p = mann_whitney_greater(&[5.5, 7.0, 8.0, 9.0, 10.0], &[1.0, 2.0, 3.0, 4.0, 6.0]);
        assert!((p - 2.0 / 252.0).abs() < 1e-12, "{p}");
        // Two out of order: P(U >= 23) = 4 / 252.
        let p = mann_whitney_greater(&[5.5, 6.5, 8.0, 9.0, 10.0], &[1.0, 2.0, 3.0, 4.0, 7.0]);
        assert!((p - 4.0 / 252.0).abs() < 1e-12, "{p}");
        // All tied: no evidence either way.
        assert_eq!(mann_whitney_greater(&[1.0; 5], &[1.0; 5]), 1.0);
        assert_eq!(mann_whitney_greater(&[], &[1.0]), 1.0);
    }

    #[test]
    fn the_normal_approximation_agrees_with_the_exact_test() {
        // 11 against 11 uses the approximation; shift one side and check
        // it against a separation that is plainly significant or not.
        let a: Vec<f64> = (0..11).map(|i| i as f64 + 20.0).collect();
        let b: Vec<f64> = (0..11).map(|i| i as f64).collect();
        let p = mann_whitney_greater(&a, &b);
        assert!(p < 1e-4, "{p}");
        let p = mann_whitney_greater(&b, &a);
        assert!(p > 0.999, "{p}");
        let mixed: Vec<f64> = (0..11).map(|i| i as f64 + 0.5).collect();
        let p = mann_whitney_greater(&mixed, &b);
        assert!(p > 0.2 && p < 0.6, "{p}");
        // erfc at known points.
        assert!((erfc(0.0) - 1.0).abs() < 1e-7);
        assert!((erfc(1.0) - 0.157_299_207).abs() < 1e-7);
        assert!((erfc(-1.0) - 1.842_700_793).abs() < 1e-7);
        let all_tied = vec![2.0; 11];
        assert_eq!(mann_whitney_greater(&all_tied, &all_tied), 1.0);
    }

    #[test]
    fn an_unchanged_candidate_passes() {
        let config = GateConfig::from_toml(CONFIG).unwrap();
        let base = baseline(&config);
        let same = runs(
            &[1.01, 0.99, 1.0, 1.03, 0.97],
            &[10_020.0, 9_980.0, 10_000.0, 9_900.0, 10_080.0],
        );
        let verdict = evaluate(&config, &base, CPU, &same, Some(5_000_000)).unwrap();
        assert!(verdict.pass, "{verdict:?}");
        assert!(verdict.failing.is_empty());
        assert!(verdict.markdown().contains("## Benchmark gate: pass"));
    }

    #[test]
    fn a_ten_percent_slowdown_fails_on_both_metrics() {
        let config = GateConfig::from_toml(CONFIG).unwrap();
        let base = baseline(&config);
        let slow = runs(
            &[1.10, 1.12, 1.09, 1.11, 1.13],
            &[9_000.0, 9_100.0, 8_950.0, 9_050.0, 9_000.0],
        );
        let verdict = evaluate(&config, &base, CPU, &slow, Some(5_000_000)).unwrap();
        assert!(!verdict.pass);
        assert_eq!(verdict.failing, ["p99_ms", "requests_per_cpu_second"]);
        let p99 = &verdict.metrics[0];
        assert!((p99.worse_by - 0.11).abs() < 1e-9, "{p99:?}");
        assert!((p99.p_value - 1.0 / 252.0).abs() < 1e-12);
        let markdown = verdict.markdown();
        assert!(
            markdown.contains("FAIL")
                && markdown
                    .contains("| p99_ms | 1.0000 | 1.1100 | +11.0% | 5.0% | 0.0040 | regressed |"),
            "{markdown}"
        );
        let json = serde_json::to_string(&verdict).unwrap();
        assert_eq!(serde_json::from_str::<Verdict>(&json).unwrap(), verdict);
    }

    #[test]
    fn small_or_insignificant_changes_pass() {
        let config = GateConfig::from_toml(CONFIG).unwrap();
        let base = baseline(&config);
        // 3% worse, consistently: significant but under the tolerance.
        let small = runs(
            &[1.03, 1.04, 1.035, 1.045, 1.05],
            &[9_700.0, 9_690.0, 9_710.0, 9_705.0, 9_695.0],
        );
        assert!(evaluate(&config, &base, CPU, &small, Some(1)).unwrap().pass);
        // 20% worse in median, but two runs better than every baseline
        // run: not significant.
        let noisy = runs(
            &[0.90, 0.95, 1.2, 1.25, 1.3],
            &[11_000.0, 10_500.0, 8_000.0, 8_100.0, 7_900.0],
        );
        let verdict = evaluate(&config, &base, CPU, &noisy, Some(1)).unwrap();
        assert!(verdict.pass, "{verdict:?}");
        assert!(
            verdict
                .metrics
                .iter()
                .all(|m| m.p_value >= 0.01 && m.worse_by > 0.05)
        );
        // Better is never a regression.
        let fast = runs(&[0.5; 5], &[20_000.0; 5]);
        assert!(evaluate(&config, &base, CPU, &fast, Some(1)).unwrap().pass);
    }

    #[test]
    fn only_listed_models_use_the_committed_samples() {
        let config =
            GateConfig::from_toml(&format!("committed_models = [\"{CPU}\"]\n{CONFIG}")).unwrap();
        assert_eq!(config.plan(CPU), "committed");
        assert_eq!(config.plan(&format!(" {CPU} ")), "committed");
        assert_eq!(config.plan("AMD EPYC 9V45 96-Core Processor"), "measure");
        assert_eq!(GateConfig::from_toml(CONFIG).unwrap().plan(CPU), "measure");
        assert_eq!(
            config.plan_line("other"),
            "measure 0f4230eb560a25a1e5e06d27b6a9d206d949de1b"
        );
        // The baseline commit's runs, under their own target name.
        let mut base = Baseline::new(&config);
        let mut runs_b = runs(&[1.0; 5], &[10_000.0; 5]);
        for r in &mut runs_b {
            r.target.name = "edgefront-base".into();
        }
        assert!(
            base.record(&config, CPU, &runs_b, "x")
                .unwrap_err()
                .contains("0 results")
        );
        base.record_target(&config, "edgefront-base", CPU, &runs_b, "x")
            .unwrap();
        assert_eq!(base.machines[CPU].samples["p99_ms"], vec![1.0; 5]);
    }

    #[test]
    fn another_cpu_model_is_not_judged_but_sizes_are() {
        let config = GateConfig::from_toml(CONFIG).unwrap();
        let base = baseline(&config);
        let slow = runs(&[2.0; 5], &[5_000.0; 5]);
        let verdict = evaluate(&config, &base, "Some Other CPU", &slow, Some(1)).unwrap();
        assert!(
            verdict.pass && !verdict.judged && verdict.metrics.is_empty(),
            "{verdict:?}"
        );
        assert!(
            verdict.markdown().contains("speed not judged")
                && verdict.markdown().contains("Some Other CPU")
        );
        let verdict = evaluate(&config, &base, "Some Other CPU", &slow, Some(10_000_001)).unwrap();
        assert!(!verdict.pass);
        assert!(verdict.markdown().starts_with("## Benchmark gate: FAIL"));
    }

    #[test]
    fn a_larger_baseline_is_judged_exactly() {
        // 5 against 20: C(25, 5) = 53,130 orderings, counted.
        let low: Vec<f64> = (0..20).map(|i| i as f64).collect();
        let p = mann_whitney_greater(&[20.0, 21.0, 22.0, 23.0, 24.0], &low);
        assert!((p - 1.0 / 53_130.0).abs() < 1e-12, "{p}");
    }

    #[test]
    fn ceilings_fail_on_their_own() {
        let config = GateConfig::from_toml(CONFIG).unwrap();
        let base = baseline(&config);
        let same = runs(&[1.0; 5], &[10_000.0; 5]);
        let verdict = evaluate(&config, &base, CPU, &same, Some(10_000_001)).unwrap();
        assert_eq!(verdict.failing, ["binary_bytes"]);
        assert!(
            verdict
                .markdown()
                .contains("| binary_bytes | 10,000,001 | 10,000,000 | over |")
        );
        let mut big = same.clone();
        big[2].target.image_size_bytes = 20_000_001;
        assert_eq!(
            evaluate(&config, &base, CPU, &big, Some(1))
                .unwrap()
                .failing,
            ["image_bytes"]
        );
        assert!(
            evaluate(&config, &base, CPU, &same, None)
                .unwrap_err()
                .contains("--binary")
        );
    }

    #[test]
    fn refuses_what_it_cannot_judge() {
        let config = GateConfig::from_toml(CONFIG).unwrap();
        let base = baseline(&config);
        let few = runs(&[1.0; 4], &[10_000.0; 4]);
        assert!(
            evaluate(&config, &base, CPU, &few, Some(1))
                .unwrap_err()
                .contains("4 results")
        );
        let mut failed = runs(&[1.0; 5], &[10_000.0; 5]);
        failed[0].measured.errors.timeout = 3;
        assert!(
            evaluate(&config, &base, CPU, &failed, Some(1))
                .unwrap_err()
                .contains("3 errors")
        );
        let mut no_cpu = runs(&[1.0; 5], &[10_000.0; 5]);
        no_cpu[1].resources.cpu_seconds = 0.0;
        assert!(
            evaluate(&config, &base, CPU, &no_cpu, Some(1))
                .unwrap_err()
                .contains("requests_per_cpu_second")
        );
        let mut thin = base.clone();
        thin.machines
            .get_mut(CPU)
            .unwrap()
            .samples
            .get_mut("p99_ms")
            .unwrap()
            .truncate(2);
        assert!(
            evaluate(&config, &thin, CPU, &runs(&[1.0; 5], &[1.0; 5]), Some(1))
                .unwrap_err()
                .contains("record it again")
        );
        for (from, to) in [
            ("runs = 5", "runs = 2"),
            ("p_value = 0.01", "p_value = 1.5"),
            ("p_value = 0.01", "p_value = 0.003"),
            ("target_cpus = 1", "target_cpus = 0"),
            ("[metrics.p99_ms]", "[metrics.p42_ms]"),
            (
                "tolerance = 0.05\n        [metrics.req",
                "tolerance = 2.0\n        [metrics.req",
            ),
            ("runs = 5", "runs = 5\n        surprise = 1"),
            ("0f4230eb560a25a1e5e06d27b6a9d206d949de1b", "0f4230e"),
        ] {
            assert!(
                GateConfig::from_toml(&CONFIG.replacen(from, to, 1)).is_err(),
                "{to}"
            );
        }
        let empty = CONFIG.split("[metrics.p99_ms]").next().unwrap().to_owned() + "[metrics]\n";
        assert!(
            GateConfig::from_toml(&empty)
                .unwrap_err()
                .contains("no [metrics]")
        );
    }

    #[test]
    fn baselines_round_trip_and_are_checked() {
        let config = GateConfig::from_toml(CONFIG).unwrap();
        assert_eq!(config.scenario_name(), "gate");
        let base = baseline(&config);
        assert_eq!(base.machines[CPU].samples["p99_ms"].len(), 5);
        let mut other = base.clone();
        assert!(
            other
                .record(&config, " ", &runs(&[1.0; 5], &[1.0; 5]), "x")
                .unwrap_err()
                .contains("empty")
        );
        other
            .record(
                &config,
                "AMD EPYC 7763 64-Core Processor",
                &runs(&[2.0; 6], &[5.0; 6]),
                "def",
            )
            .unwrap();
        assert_eq!(other.machines.len(), 2);
        assert_eq!(
            other.machines["AMD EPYC 7763 64-Core Processor"].samples["p99_ms"].len(),
            6
        );
        let dir = std::env::temp_dir().join(format!("efb-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("b.json");
        std::fs::write(&path, serde_json::to_string(&base).unwrap()).unwrap();
        assert_eq!(Baseline::load(&path).unwrap(), base);
        let mut wrong = base.clone();
        wrong.schema = "v0".into();
        std::fs::write(&path, serde_json::to_string(&wrong).unwrap()).unwrap();
        assert!(Baseline::load(&path).unwrap_err().contains("schema"));
        std::fs::write(&path, "{").unwrap();
        assert!(Baseline::load(&path).is_err());
        assert!(Baseline::load(&dir.join("absent.json")).is_err());
        let toml = dir.join("g.toml");
        std::fs::write(&toml, CONFIG).unwrap();
        assert_eq!(GateConfig::load(&toml).unwrap(), config);
        assert!(GateConfig::load(&dir.join("absent.toml")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(median(&[]).is_nan());
        assert_eq!(median(&[3.0, 1.0, 2.0, 10.0]), 2.5);
        for name in METRICS {
            assert!(metric(&result(1.0, 1.0), name).is_some(), "{name}");
        }
        assert_eq!(metric(&result(1.0, 1.0), "nope"), None);
    }
}
