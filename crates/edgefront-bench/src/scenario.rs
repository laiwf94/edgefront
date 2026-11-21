//! Benchmark scenarios, read from TOML.
//!
//! A scenario says what a run asks for and how hard: the arrival schedule
//! (an optional warm-up, then a constant or stepped rate), the protocol and
//! connection count, how many distinct objects there are and how their
//! popularity is skewed (a Zipf exponent), the mix of object sizes, and the
//! hit ratio the run should see. The files live under `bench/scenarios/`:
//!
//! ```toml
//! name = "cache-hit"
//! description = "Small objects, almost all hits."
//! protocol = "http1"        # or "http2"; default http1
//! connections = 32          # default 8
//! timeout = "5s"            # default 10s
//! seed = 1                  # default 1
//!
//! [warmup]                  # optional
//! rate = 2000
//! duration = "10s"
//!
//! [load]                    # rate + duration, or start + step + steps + step_duration
//! rate = 10000
//! duration = "30s"
//!
//! [keys]
//! count = 1000
//! zipf = 1.0                # 0 is uniform
//! hit_ratio = 0.98
//!
//! [[objects]]               # the size mix, by weight
//! size = "4KiB"
//! weight = 3
//! ```
//!
//! Requests are `GET /obj/<size>/<name>`, where `<size>` is the body
//! length in bytes the origin is to serve and `<name>` is `k<rank>` for one
//! of the `count` popular objects (rank 1 the most popular) or `u<seq>` for
//! an object asked for once. Each popular object keeps one size, drawn from
//! the mix by its rank, so a target sees the same object every time.
//!
//! **Hit ratio.** Warm-up asks only for popular objects. A measured request
//! asks for a once-only object with probability `u`, which is a miss, and
//! otherwise for a popular object drawn by the Zipf law, which is a hit if
//! that object was asked for before. [`Scenario::new`] computes the
//! expected fraction of measured popular requests that hit, from the warm-up
//! and measured request counts, and picks `u` so the expected overall hit
//! ratio is the target. A target the popular objects cannot reach (too many
//! of them for the warm-up, or too little skew) is an error, not a quietly
//! lower ratio. The prediction assumes the target keeps every object it has
//! fetched; [`Scenario::working_set`] is how many bytes that takes.
//!
//! Every request is a pure function of the scenario and its sequence
//! number, so two runs of a scenario ask for exactly the same objects in
//! the same order, whatever the workers' interleaving.
//!
//! **gRPC.** With `requests = "grpc"` (HTTP/2 only) each request is a
//! unary gRPC call instead of a `GET`: `POST` [`crate::origin::GRPC_METHOD`]
//! whose one message is the object's path, answered by one message of the
//! object's size and `grpc-status: 0` in the trailers. A call that ends
//! with any other status counts as a gRPC error.
//!
//! **Streams.** `origin_delay = "200ms"` has the origin hold every object
//! request that long before answering, so the streams a run keeps open at
//! once are about its rate times the delay (Little's law): 5,000 a second
//! held 200 ms is 1,000 concurrent streams through the target.
//!
//! **Several scenarios in one file.** A file with `[[scenarios]]` holds one
//! scenario per entry; the keys outside them are defaults every entry
//! starts from, a table such as `[load]` replaced whole by an entry's own.
//! [`Scenario::load_all`] reads them, [`Scenario::load_named`] one of them
//! by name, and [`Scenario::load`] a file of one.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::Request;
use http_body_util::Full;
use serde::Deserialize;

use crate::load::{Phase, Protocol, RequestSource, Schedule};
use crate::origin;

/// The longest `origin_delay` a scenario may ask for.
pub const MAX_ORIGIN_DELAY: Duration = Duration::from_secs(10);

/// What each request of a scenario is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    /// `GET /obj/<size>/<name>`.
    Get,
    /// A unary gRPC call for the object, over HTTP/2.
    Grpc,
}

impl RequestKind {
    /// `get` or `grpc`, as scenarios and results name it.
    pub fn name(self) -> &'static str {
        match self {
            RequestKind::Get => "get",
            RequestKind::Grpc => "grpc",
        }
    }
}

/// The most popular objects a scenario may have.
pub const MAX_KEYS: u64 = 10_000_000;
/// The largest object a scenario may ask for.
pub const MAX_OBJECT: u64 = 1 << 30;
/// How far below the target the popular objects alone may fall and still
/// count as reaching it: half a percentage point.
pub const HIT_RATIO_TOLERANCE: f64 = 0.005;

/// Why a scenario could not be read.
#[derive(Debug, Clone, PartialEq)]
pub enum ScenarioError {
    /// The file could not be read.
    Io { path: PathBuf, reason: String },
    /// The TOML is malformed or does not have the scenario's shape.
    Parse(String),
    /// A field has a value the harness cannot run.
    Invalid { field: &'static str, reason: String },
}

impl fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScenarioError::Io { path, reason } => write!(f, "{}: {reason}", path.display()),
            ScenarioError::Parse(reason) => write!(f, "{reason}"),
            ScenarioError::Invalid { field, reason } => write!(f, "{field}: {reason}"),
        }
    }
}

impl std::error::Error for ScenarioError {}

fn invalid(field: &'static str, reason: impl Into<String>) -> ScenarioError {
    ScenarioError::Invalid {
        field,
        reason: reason.into(),
    }
}

/// The arrival rate after warm-up.
#[derive(Clone, Debug, PartialEq)]
pub enum Load {
    /// One rate held for a duration.
    Constant { rate: f64, duration: Duration },
    /// `steps` phases of `step_duration`, from `start` requests per second,
    /// each `step` higher than the one before.
    Stepped {
        start: f64,
        step: f64,
        steps: usize,
        step_duration: Duration,
    },
}

/// The popular objects.
#[derive(Clone, Debug, PartialEq)]
pub struct Keys {
    /// How many there are.
    pub count: u64,
    /// The Zipf exponent of their popularity: rank `r` is asked for in
    /// proportion to `1 / r^zipf`.
    pub zipf: f64,
    /// The fraction of measured requests that should hit.
    pub hit_ratio: f64,
}

/// One class of the size mix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectClass {
    /// The body length in bytes.
    pub size: u64,
    /// Its share of objects, relative to the other classes' weights.
    pub weight: u64,
}

/// A validated scenario.
#[derive(Clone, Debug, PartialEq)]
pub struct Scenario {
    /// The scenario's name in reports.
    pub name: String,
    /// What it measures, in a sentence.
    pub description: String,
    /// The HTTP version.
    pub protocol: Protocol,
    /// How many connections the generator opens.
    pub connections: usize,
    /// How long after its due time a request may take.
    pub timeout: Duration,
    /// Seeds every draw; another seed asks for other objects.
    pub seed: u64,
    /// The warm-up rate and duration, if any.
    pub warmup: Option<(f64, Duration)>,
    /// The measured load.
    pub load: Load,
    /// The popular objects.
    pub keys: Keys,
    /// The size mix.
    pub objects: Vec<ObjectClass>,
    /// Load the target over TLS with this SNI; `None` is cleartext.
    pub tls: Option<String>,
    /// `GET`s or unary gRPC calls.
    pub kind: RequestKind,
    /// How long the origin holds each object request before answering.
    pub origin_delay: Duration,
    /// The probability a measured request asks for a once-only object.
    once_only: f64,
    /// The expected hit ratio of the measured requests.
    expected_hit_ratio: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScenario {
    name: String,
    #[serde(default)]
    description: String,
    protocol: Option<String>,
    connections: Option<i64>,
    timeout: Option<String>,
    seed: Option<i64>,
    warmup: Option<RawWarmup>,
    load: RawLoad,
    keys: RawKeys,
    objects: Vec<RawObject>,
    tls: Option<RawTls>,
    requests: Option<String>,
    origin_delay: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTls {
    server_name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWarmup {
    rate: f64,
    duration: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLoad {
    rate: Option<f64>,
    duration: Option<String>,
    start: Option<f64>,
    step: Option<f64>,
    steps: Option<i64>,
    step_duration: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKeys {
    count: i64,
    #[serde(default)]
    zipf: f64,
    hit_ratio: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawObject {
    size: RawSize,
    #[serde(default = "one")]
    weight: i64,
}

fn one() -> i64 {
    1
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawSize {
    Bytes(i64),
    Text(String),
}

impl Scenario {
    /// Reads and validates a scenario file.
    pub fn load(path: impl AsRef<Path>) -> Result<Scenario, ScenarioError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| ScenarioError::Io {
            path: path.to_owned(),
            reason: e.to_string(),
        })?;
        Scenario::from_toml(&text).map_err(|e| match e {
            ScenarioError::Parse(reason) => {
                ScenarioError::Parse(format!("{}: {reason}", path.display()))
            }
            other => other,
        })
    }

    /// Reads every `*.toml` scenario in a directory, in file name order.
    pub fn load_dir(dir: impl AsRef<Path>) -> Result<Vec<Scenario>, ScenarioError> {
        let dir = dir.as_ref();
        let io = |e: std::io::Error| ScenarioError::Io {
            path: dir.to_owned(),
            reason: e.to_string(),
        };
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(dir).map_err(io)? {
            let path = entry.map_err(io)?.path();
            if path.extension().is_some_and(|e| e == "toml") {
                paths.push(path);
            }
        }
        paths.sort();
        paths.iter().map(Scenario::load).collect()
    }

    /// Reads and validates every scenario in a file: each entry of its
    /// `[[scenarios]]`, or the file itself when it has none.
    pub fn load_all(path: impl AsRef<Path>) -> Result<Vec<Scenario>, ScenarioError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| ScenarioError::Io {
            path: path.to_owned(),
            reason: e.to_string(),
        })?;
        Scenario::all_from_toml(&text).map_err(|e| match e {
            ScenarioError::Parse(reason) => {
                ScenarioError::Parse(format!("{}: {reason}", path.display()))
            }
            other => other,
        })
    }

    /// The scenario named `name` in a file of one or several.
    pub fn load_named(path: impl AsRef<Path>, name: &str) -> Result<Scenario, ScenarioError> {
        let path = path.as_ref();
        Scenario::load_all(path)?
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| {
                invalid(
                    "name",
                    format!("{} has no scenario {name:?}", path.display()),
                )
            })
    }

    /// Every scenario in `text`: each `[[scenarios]]` entry over the keys
    /// outside them, or `text` itself when it has no entries.
    pub fn all_from_toml(text: &str) -> Result<Vec<Scenario>, ScenarioError> {
        let parse = |e: toml::de::Error| ScenarioError::Parse(e.to_string());
        let mut table: toml::Table = toml::from_str(text).map_err(parse)?;
        let Some(entries) = table.remove("scenarios") else {
            return Ok(vec![Scenario::from_toml(text)?]);
        };
        let toml::Value::Array(entries) = entries else {
            return Err(invalid("scenarios", "must be an array of tables"));
        };
        if entries.is_empty() {
            return Err(invalid("scenarios", "has no entries"));
        }
        let mut out: Vec<Scenario> = Vec::with_capacity(entries.len());
        for entry in entries {
            let toml::Value::Table(entry) = entry else {
                return Err(invalid("scenarios", "must be an array of tables"));
            };
            let mut merged = table.clone();
            merged.extend(entry);
            let raw: RawScenario = toml::Value::Table(merged).try_into().map_err(parse)?;
            let scenario = Scenario::from_raw(raw)?;
            if out.iter().any(|s| s.name == scenario.name) {
                return Err(invalid(
                    "scenarios.name",
                    format!("{:?} names two scenarios", scenario.name),
                ));
            }
            out.push(scenario);
        }
        Ok(out)
    }

    /// Parses and validates a scenario.
    pub fn from_toml(text: &str) -> Result<Scenario, ScenarioError> {
        let raw: RawScenario = toml::from_str(text).map_err(|e| {
            if toml::from_str::<toml::Table>(text).is_ok_and(|t| t.contains_key("scenarios")) {
                ScenarioError::Parse(
                    "holds several scenarios ([[scenarios]]); read it with Scenario::load_all"
                        .into(),
                )
            } else {
                ScenarioError::Parse(e.to_string())
            }
        })?;
        Scenario::from_raw(raw)
    }

    fn from_raw(raw: RawScenario) -> Result<Scenario, ScenarioError> {
        if raw.name.is_empty()
            || !raw
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(invalid(
                "name",
                "must be non-empty letters, digits, '-' and '_'",
            ));
        }
        let protocol = match raw.protocol.as_deref() {
            None | Some("http1") => Protocol::Http1,
            Some("http2") => Protocol::Http2,
            Some(other) => {
                return Err(invalid(
                    "protocol",
                    format!("{other:?} is not \"http1\" or \"http2\""),
                ));
            }
        };
        let connections = match raw.connections {
            None => 8,
            Some(n) if (1..=65_536).contains(&n) => n as usize,
            Some(n) => return Err(invalid("connections", format!("{n} is not 1 to 65536"))),
        };
        let timeout = match raw.timeout {
            None => Duration::from_secs(10),
            Some(t) => positive_duration("timeout", &t)?,
        };
        let seed = match raw.seed {
            None => 1,
            Some(s) => s as u64,
        };
        let warmup = match raw.warmup {
            None => None,
            Some(w) => Some((
                positive_rate("warmup.rate", w.rate)?,
                positive_duration("warmup.duration", &w.duration)?,
            )),
        };
        let load = Scenario::load_from_raw(raw.load)?;

        let count = raw.keys.count;
        if !(1..=MAX_KEYS as i64).contains(&count) {
            return Err(invalid(
                "keys.count",
                format!("{count} is not 1 to {MAX_KEYS}"),
            ));
        }
        let zipf = raw.keys.zipf;
        if !(0.0..=10.0).contains(&zipf) {
            return Err(invalid("keys.zipf", format!("{zipf} is not 0 to 10")));
        }
        let hit_ratio = raw.keys.hit_ratio;
        if !(0.0..=1.0).contains(&hit_ratio) {
            return Err(invalid(
                "keys.hit_ratio",
                format!("{hit_ratio} is not 0 to 1"),
            ));
        }
        let keys = Keys {
            count: count as u64,
            zipf,
            hit_ratio,
        };

        if raw.objects.is_empty() {
            return Err(invalid("objects", "needs at least one size class"));
        }
        let mut objects = Vec::with_capacity(raw.objects.len());
        for object in raw.objects {
            let size = match object.size {
                RawSize::Bytes(n) if n >= 0 => n as u64,
                RawSize::Bytes(n) => {
                    return Err(invalid("objects.size", format!("{n} is negative")));
                }
                RawSize::Text(text) => parse_size(&text)
                    .ok_or_else(|| invalid("objects.size", format!("{text:?} is not a size")))?,
            };
            if size > MAX_OBJECT {
                return Err(invalid(
                    "objects.size",
                    format!("{size} bytes is over 1 GiB"),
                ));
            }
            if !(1..=1_000_000).contains(&object.weight) {
                return Err(invalid(
                    "objects.weight",
                    format!("{} is not 1 to 1000000", object.weight),
                ));
            }
            objects.push(ObjectClass {
                size,
                weight: object.weight as u64,
            });
        }

        let tls = match raw.tls {
            None => None,
            Some(t) => {
                rustls::pki_types::ServerName::try_from(t.server_name.as_str()).map_err(|_| {
                    invalid(
                        "tls.server_name",
                        format!("{:?} is not a DNS name or IP address", t.server_name),
                    )
                })?;
                Some(t.server_name)
            }
        };
        let kind = match raw.requests.as_deref() {
            None | Some("get") => RequestKind::Get,
            Some("grpc") => RequestKind::Grpc,
            Some(other) => {
                return Err(invalid(
                    "requests",
                    format!("{other:?} is not \"get\" or \"grpc\""),
                ));
            }
        };
        if kind == RequestKind::Grpc && protocol != Protocol::Http2 {
            return Err(invalid("requests", "gRPC needs protocol = \"http2\""));
        }
        let origin_delay = match raw.origin_delay {
            None => Duration::ZERO,
            Some(text) => match parse_duration(&text) {
                Some(d) if d <= MAX_ORIGIN_DELAY => d,
                Some(_) => return Err(invalid("origin_delay", "must be at most 10s")),
                None => {
                    return Err(invalid(
                        "origin_delay",
                        format!("{text:?} is not a duration like 500ms, 10s or 2m"),
                    ));
                }
            },
        };
        let mut scenario = Scenario {
            tls,
            kind,
            origin_delay,
            name: raw.name,
            description: raw.description,
            protocol,
            connections,
            timeout,
            seed,
            warmup,
            load,
            keys,
            objects,
            once_only: 0.0,
            expected_hit_ratio: 0.0,
        };
        scenario.solve_hit_ratio()?;
        Ok(scenario)
    }

    fn load_from_raw(raw: RawLoad) -> Result<Load, ScenarioError> {
        let constant = raw.rate.is_some() || raw.duration.is_some();
        let stepped = raw.start.is_some()
            || raw.step.is_some()
            || raw.steps.is_some()
            || raw.step_duration.is_some();
        match (constant, stepped) {
            (true, false) => {
                let (Some(rate), Some(duration)) = (raw.rate, raw.duration) else {
                    return Err(invalid("load", "a constant load needs rate and duration"));
                };
                Ok(Load::Constant {
                    rate: positive_rate("load.rate", rate)?,
                    duration: positive_duration("load.duration", &duration)?,
                })
            }
            (false, true) => {
                let (Some(start), Some(step), Some(steps), Some(step_duration)) =
                    (raw.start, raw.step, raw.steps, raw.step_duration)
                else {
                    return Err(invalid(
                        "load",
                        "a stepped load needs start, step, steps and step_duration",
                    ));
                };
                let start = positive_rate("load.start", start)?;
                if !step.is_finite() {
                    return Err(invalid("load.step", "must be a finite number"));
                }
                if !(1..=1000).contains(&steps) {
                    return Err(invalid("load.steps", format!("{steps} is not 1 to 1000")));
                }
                let last = start + step * (steps - 1) as f64;
                positive_rate("load.step", last)
                    .map_err(|_| invalid("load.step", "the last step's rate must be positive"))?;
                Ok(Load::Stepped {
                    start,
                    step,
                    steps: steps as usize,
                    step_duration: positive_duration("load.step_duration", &step_duration)?,
                })
            }
            (true, true) => Err(invalid(
                "load",
                "has both a constant rate and steps; pick one",
            )),
            (false, false) => Err(invalid(
                "load",
                "needs rate and duration, or start, step, steps and step_duration",
            )),
        }
    }

    /// The arrival schedule: warm-up first, if any, then the load.
    pub fn schedule(&self) -> Schedule {
        let schedule = match self.load {
            Load::Constant { rate, duration } => {
                Schedule::new(vec![Phase::new("load", rate, duration)])
            }
            Load::Stepped {
                start,
                step,
                steps,
                step_duration,
            } => Schedule::stepped(start, step, steps, step_duration),
        };
        match self.warmup {
            Some((rate, duration)) => schedule.with_warmup(rate, duration),
            None => schedule,
        }
    }

    /// How many warm-up requests the run sends.
    pub fn warmup_requests(&self) -> u64 {
        self.schedule()
            .phases()
            .iter()
            .filter(|p| p.warmup)
            .map(Phase::requests)
            .sum()
    }

    /// How many measured requests the run sends.
    pub fn measured_requests(&self) -> u64 {
        self.schedule()
            .phases()
            .iter()
            .filter(|p| !p.warmup)
            .map(Phase::requests)
            .sum()
    }

    /// The probability that a measured request asks for a once-only
    /// object.
    pub fn once_only(&self) -> f64 {
        self.once_only
    }

    /// The hit ratio the measured requests are expected to see against a
    /// target that keeps everything it fetched: the target, or within
    /// [`HIT_RATIO_TOLERANCE`] below it.
    pub fn expected_hit_ratio(&self) -> f64 {
        self.expected_hit_ratio
    }

    /// The bytes the popular objects take: what a target needs to keep
    /// them all.
    pub fn working_set(&self) -> u64 {
        let total: u64 = self.objects.iter().map(|o| o.weight).sum();
        // For many objects, the expected size times the count is exact to
        // well under a percent and costs nothing.
        if self.keys.count > 100_000 {
            let mean = self
                .objects
                .iter()
                .map(|o| o.size as f64 * o.weight as f64)
                .sum::<f64>()
                / total as f64;
            return (mean * self.keys.count as f64).round() as u64;
        }
        (1..=self.keys.count).map(|rank| self.key_size(rank)).sum()
    }

    /// The request source that asks for this scenario's objects.
    pub fn requests(&self) -> Arc<ScenarioRequests> {
        Arc::new(ScenarioRequests::new(self))
    }

    /// The size of the popular object of rank `rank`.
    fn key_size(&self, rank: u64) -> u64 {
        pick_size(&self.objects, uniform(self.seed, STREAM_KEY_SIZE, rank))
    }

    /// Picks the once-only probability that brings the expected hit ratio
    /// to the target, or fails if the popular objects cannot reach it.
    fn solve_hit_ratio(&mut self) -> Result<(), ScenarioError> {
        let target = self.keys.hit_ratio;
        let warmup = self.warmup_requests();
        let measured = self.measured_requests();
        let probabilities = zipf_probabilities(self.keys.count, self.keys.zipf);
        // The popular requests' hit ratio depends on how many popular
        // requests there are, which depends on the once-only probability;
        // the fixed point converges in a few rounds, as the ratio moves
        // little with the count.
        let mut once_only = 0.0;
        let mut popular_hit = 0.0;
        for _ in 0..8 {
            let popular = measured as f64 * (1.0 - once_only);
            popular_hit = popular_hit_ratio(&probabilities, warmup as f64, popular);
            once_only = if popular_hit <= 0.0 {
                1.0
            } else {
                (1.0 - target / popular_hit).clamp(0.0, 1.0)
            };
        }
        if target > 0.0 && popular_hit + HIT_RATIO_TOLERANCE < target {
            return Err(invalid(
                "keys.hit_ratio",
                format!(
                    "{target} is out of reach: the popular objects hit {popular_hit:.4} of \
                     the time after {warmup} warm-up requests; lengthen the warm-up, \
                     lower keys.count or raise keys.zipf"
                ),
            ));
        }
        self.once_only = once_only;
        self.expected_hit_ratio = (1.0 - once_only) * popular_hit;
        Ok(())
    }
}

/// The expected fraction of `measured` popular requests that hit, after
/// `warmup` popular requests, for objects asked for with `probabilities`.
/// Object `k` is a miss on a request only if no earlier request asked for
/// it: the `j`th measured request misses it with probability
/// `(1 - p)^(warmup + j)`, and averaging that over `j` gives the sum's
/// geometric series.
fn popular_hit_ratio(probabilities: &[f64], warmup: f64, measured: f64) -> f64 {
    let mut miss = 0.0;
    for &p in probabilities {
        if p <= 0.0 {
            continue;
        }
        let q = 1.0 - p;
        let before = q.powf(warmup);
        let unseen = if measured >= 1.0 {
            before * (1.0 - q.powf(measured)) / (measured * p)
        } else {
            before
        };
        miss += p * unseen;
    }
    (1.0 - miss).clamp(0.0, 1.0)
}

/// The probability of each rank, 1 first, under a Zipf law.
fn zipf_probabilities(count: u64, exponent: f64) -> Vec<f64> {
    let mut weights: Vec<f64> = (1..=count).map(|r| (r as f64).powf(-exponent)).collect();
    let total: f64 = weights.iter().sum();
    for w in &mut weights {
        *w /= total;
    }
    weights
}

/// Draw streams, so the draws for one sequence number are independent.
const STREAM_ONCE_ONLY: u64 = 1;
const STREAM_RANK: u64 = 2;
const STREAM_KEY_SIZE: u64 = 3;
const STREAM_ONCE_SIZE: u64 = 4;

/// SplitMix64's finaliser: a bijection of u64 whose outputs pass BigCrush.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A uniform draw in `[0, 1)` for `(seed, stream, n)`.
fn uniform(seed: u64, stream: u64, n: u64) -> f64 {
    let bits = mix(mix(mix(seed) ^ stream) ^ n);
    (bits >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
}

fn pick_size(objects: &[ObjectClass], u: f64) -> u64 {
    let total: u64 = objects.iter().map(|o| o.weight).sum();
    let mut target = (u * total as f64) as u64;
    for object in objects {
        if target < object.weight {
            return object.size;
        }
        target -= object.weight;
    }
    objects[objects.len() - 1].size
}

/// The requests of a scenario, numbered from the first warm-up request.
#[derive(Debug)]
pub struct ScenarioRequests {
    seed: u64,
    objects: Vec<ObjectClass>,
    /// The cumulative probability of each rank, the last one 1.
    cdf: Vec<f64>,
    warmup: u64,
    once_only: f64,
    kind: RequestKind,
}

/// What one request asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Object {
    /// The popular object of this rank, 1 the most popular.
    Popular { rank: u64, size: u64 },
    /// The object only request `seq` asks for.
    OnceOnly { seq: u64, size: u64 },
}

impl Object {
    /// The request path: `/obj/<size>/k<rank>` or `/obj/<size>/u<seq>`.
    pub fn path(&self) -> String {
        match self {
            Object::Popular { rank, size } => format!("/obj/{size}/k{rank}"),
            Object::OnceOnly { seq, size } => format!("/obj/{size}/u{seq}"),
        }
    }

    /// The body length the origin serves.
    pub fn size(&self) -> u64 {
        match self {
            Object::Popular { size, .. } | Object::OnceOnly { size, .. } => *size,
        }
    }
}

impl ScenarioRequests {
    fn new(scenario: &Scenario) -> Self {
        let mut cdf = zipf_probabilities(scenario.keys.count, scenario.keys.zipf);
        let mut sum = 0.0;
        for p in &mut cdf {
            sum += *p;
            *p = sum;
        }
        if let Some(last) = cdf.last_mut() {
            *last = 1.0;
        }
        ScenarioRequests {
            seed: scenario.seed,
            objects: scenario.objects.clone(),
            cdf,
            warmup: scenario.warmup_requests(),
            once_only: scenario.once_only,
            kind: scenario.kind,
        }
    }

    /// The object request `seq` asks for.
    pub fn object(&self, seq: u64) -> Object {
        if seq >= self.warmup && uniform(self.seed, STREAM_ONCE_ONLY, seq) < self.once_only {
            let size = pick_size(&self.objects, uniform(self.seed, STREAM_ONCE_SIZE, seq));
            return Object::OnceOnly { seq, size };
        }
        let u = uniform(self.seed, STREAM_RANK, seq);
        let index = self
            .cdf
            .partition_point(|&c| c <= u)
            .min(self.cdf.len() - 1);
        let rank = index as u64 + 1;
        let size = pick_size(&self.objects, uniform(self.seed, STREAM_KEY_SIZE, rank));
        Object::Popular { rank, size }
    }
}

impl RequestSource for ScenarioRequests {
    fn request(&self, seq: u64) -> Request<Full<Bytes>> {
        let path = self.object(seq).path();
        match self.kind {
            RequestKind::Get => {
                let mut request = Request::new(Full::new(Bytes::new()));
                // The path is ASCII digits and letters, so it always parses.
                *request.uri_mut() = path.parse().expect("a valid path");
                request
            }
            RequestKind::Grpc => origin::grpc_request(&path),
        }
    }
}

/// Parses `500ms`, `10s`, `2m`, `1h` or a bare number of seconds, with an
/// optional fraction (`1.5s`).
pub fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    if number.is_empty() || number.starts_with('.') || number.ends_with('.') {
        return None;
    }
    let value: f64 = number.parse().ok()?;
    let scale = match unit.trim() {
        "ms" => 1e-3,
        "" | "s" => 1.0,
        "m" | "min" => 60.0,
        "h" => 3600.0,
        _ => return None,
    };
    Duration::try_from_secs_f64(value * scale).ok()
}

/// Parses a byte count: `512`, `512B`, `4KiB`, `1.5MiB`, `1GiB`, or the
/// decimal `kB`, `MB` and `GB`.
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    if number.is_empty() || number.starts_with('.') || number.ends_with('.') {
        return None;
    }
    let value: f64 = number.parse().ok()?;
    let scale: f64 = match unit.trim() {
        "" | "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "kB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        _ => return None,
    };
    let bytes = value * scale;
    if bytes.fract() != 0.0 || bytes > u64::MAX as f64 {
        return None;
    }
    Some(bytes as u64)
}

fn positive_duration(field: &'static str, text: &str) -> Result<Duration, ScenarioError> {
    match parse_duration(text) {
        Some(d) if !d.is_zero() => Ok(d),
        Some(_) => Err(invalid(field, "must be longer than zero")),
        None => Err(invalid(
            field,
            format!("{text:?} is not a duration like 500ms, 10s or 2m"),
        )),
    }
}

fn positive_rate(field: &'static str, rate: f64) -> Result<f64, ScenarioError> {
    if rate.is_finite() && rate > 0.0 && rate <= 10_000_000.0 {
        Ok(rate)
    } else {
        Err(invalid(
            field,
            format!("{rate} is not above 0 and at most 10,000,000 per second"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const BASIC: &str = r#"
        name = "basic"
        description = "A test."
        connections = 4
        timeout = "2s"
        seed = 7

        [warmup]
        rate = 1000
        duration = "2s"

        [load]
        rate = 2000
        duration = "10s"

        [keys]
        count = 1000
        zipf = 0.9
        hit_ratio = 0.8

        [[objects]]
        size = "1KiB"
        weight = 3

        [[objects]]
        size = 65536
        weight = 1
    "#;

    fn with(text: &str, from: &str, to: &str) -> String {
        assert!(text.contains(from), "{from:?} not in the scenario");
        text.replacen(from, to, 1)
    }

    fn error(text: &str) -> ScenarioError {
        Scenario::from_toml(text).unwrap_err()
    }

    fn field(text: &str) -> &'static str {
        match error(text) {
            ScenarioError::Invalid { field, .. } => field,
            other => panic!("expected an invalid field, got {other:?}"),
        }
    }

    #[test]
    fn reads_every_field() {
        let s = Scenario::from_toml(BASIC).unwrap();
        assert_eq!(s.name, "basic");
        assert_eq!(s.description, "A test.");
        assert_eq!(s.protocol, Protocol::Http1);
        assert_eq!(s.connections, 4);
        assert_eq!(s.timeout, Duration::from_secs(2));
        assert_eq!(s.seed, 7);
        assert_eq!(s.warmup, Some((1000.0, Duration::from_secs(2))));
        assert_eq!(
            s.load,
            Load::Constant {
                rate: 2000.0,
                duration: Duration::from_secs(10)
            }
        );
        assert_eq!(
            s.keys,
            Keys {
                count: 1000,
                zipf: 0.9,
                hit_ratio: 0.8
            }
        );
        assert_eq!(
            s.objects,
            vec![
                ObjectClass {
                    size: 1024,
                    weight: 3
                },
                ObjectClass {
                    size: 65536,
                    weight: 1
                }
            ]
        );
        assert_eq!(s.warmup_requests(), 2000);
        assert_eq!(s.measured_requests(), 20_000);
        let phases = s.schedule();
        assert_eq!(phases.phases().len(), 2);
        assert!(phases.phases()[0].warmup);
        assert_eq!(phases.duration(), Duration::from_secs(12));
        assert!((s.expected_hit_ratio() - 0.8).abs() < 1e-9);
        assert!(s.once_only() > 0.0 && s.once_only() < 1.0);
    }

    #[test]
    fn fills_in_defaults() {
        let s = Scenario::from_toml(
            r#"
            name = "min"
            [load]
            rate = 100
            duration = "1s"
            [keys]
            count = 1
            hit_ratio = 0
            [[objects]]
            size = "0"
            "#,
        )
        .unwrap();
        assert_eq!(s.protocol, Protocol::Http1);
        assert_eq!(s.connections, 8);
        assert_eq!(s.timeout, Duration::from_secs(10));
        assert_eq!(s.seed, 1);
        assert_eq!(s.warmup, None);
        assert_eq!(s.keys.zipf, 0.0);
        assert_eq!(s.objects[0].weight, 1);
        assert_eq!(s.description, "");
        // A target of 0 sends every measured request to a once-only object.
        assert_eq!(s.once_only(), 1.0);
        assert_eq!(s.expected_hit_ratio(), 0.0);
        let requests = s.requests();
        assert!(matches!(
            requests.object(0),
            Object::OnceOnly { seq: 0, .. }
        ));
    }

    #[test]
    fn reads_a_stepped_load_over_http2() {
        let text = with(
            &with(
                BASIC,
                "connections = 4",
                "protocol = \"http2\"\nconnections = 4",
            ),
            "rate = 2000\n        duration = \"10s\"",
            "start = 500\n        step = 500\n        steps = 4\n        step_duration = \"5s\"",
        );
        let s = Scenario::from_toml(&text).unwrap();
        assert_eq!(s.protocol, Protocol::Http2);
        let schedule = s.schedule();
        let rates: Vec<f64> = schedule.phases().iter().map(|p| p.rate).collect();
        assert_eq!(rates, vec![1000.0, 500.0, 1000.0, 1500.0, 2000.0]);
        assert_eq!(s.measured_requests(), 5 * (500 + 1000 + 1500 + 2000));
    }

    #[test]
    fn without_warm_up_the_schedule_is_the_load_alone() {
        let text = with(
            BASIC,
            "[warmup]\n        rate = 1000\n        duration = \"2s\"",
            "",
        );
        let text = with(&text, "hit_ratio = 0.8", "hit_ratio = 0.5");
        let s = Scenario::from_toml(&text).unwrap();
        assert_eq!(s.schedule().phases().len(), 1);
        assert_eq!(s.warmup_requests(), 0);
        assert!((s.expected_hit_ratio() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn rejects_bad_fields() {
        let cases: &[(&str, &str, &str)] = &[
            ("name = \"basic\"", "name = \"two words\"", "name"),
            ("name = \"basic\"", "name = \"\"", "name"),
            ("connections = 4", "protocol = \"h3\"", "protocol"),
            ("connections = 4", "connections = 0", "connections"),
            ("connections = 4", "connections = 70000", "connections"),
            ("timeout = \"2s\"", "timeout = \"soon\"", "timeout"),
            ("timeout = \"2s\"", "timeout = \"0s\"", "timeout"),
            ("rate = 1000", "rate = 0", "warmup.rate"),
            ("rate = 1000", "rate = nan", "warmup.rate"),
            ("duration = \"2s\"", "duration = \"2x\"", "warmup.duration"),
            ("rate = 2000", "rate = -1", "load.rate"),
            ("rate = 2000", "rate = 2e7", "load.rate"),
            ("duration = \"10s\"", "duration = \"\"", "load.duration"),
            ("count = 1000", "count = 0", "keys.count"),
            ("count = 1000", "count = 20000000", "keys.count"),
            ("zipf = 0.9", "zipf = -1", "keys.zipf"),
            ("zipf = 0.9", "zipf = 11", "keys.zipf"),
            ("hit_ratio = 0.8", "hit_ratio = 1.5", "keys.hit_ratio"),
            ("size = \"1KiB\"", "size = \"1 parsec\"", "objects.size"),
            ("size = \"1KiB\"", "size = \"2GiB\"", "objects.size"),
            ("size = 65536", "size = -1", "objects.size"),
            ("weight = 3", "weight = 0", "objects.weight"),
        ];
        for (from, to, expected) in cases {
            assert_eq!(field(&with(BASIC, from, to)), *expected, "{to}");
        }
    }

    #[test]
    fn rejects_a_load_that_is_both_or_neither() {
        let both = with(BASIC, "rate = 2000", "rate = 2000\n        steps = 3");
        assert_eq!(field(&both), "load");
        let neither = with(
            BASIC,
            "rate = 2000\n        duration = \"10s\"",
            "# nothing",
        );
        assert_eq!(field(&neither), "load");
        let half = with(BASIC, "duration = \"10s\"", "");
        assert_eq!(field(&half), "load");
        let half_stepped = with(
            BASIC,
            "rate = 2000\n        duration = \"10s\"",
            "start = 100\n        steps = 3",
        );
        assert_eq!(field(&half_stepped), "load");
        let stepped = |start: &str, step: &str, steps: &str| {
            with(
                BASIC,
                "rate = 2000\n        duration = \"10s\"",
                &format!(
                    "start = {start}\n step = {step}\n steps = {steps}\n step_duration = \"1s\""
                ),
            )
        };
        assert_eq!(field(&stepped("100", "nan", "3")), "load.step");
        assert_eq!(field(&stepped("100", "-100", "3")), "load.step");
        assert_eq!(field(&stepped("100", "10", "0")), "load.steps");
        assert_eq!(field(&stepped("0", "10", "3")), "load.start");
        // Falling steps are fine while the last stays positive.
        Scenario::from_toml(&stepped("300", "-100", "3")).unwrap();
    }

    #[test]
    fn rejects_malformed_and_unknown_fields() {
        assert!(matches!(error("name = "), ScenarioError::Parse(_)));
        let unknown = with(BASIC, "seed = 7", "seed = 7\n        sede = 8");
        let ScenarioError::Parse(reason) = error(&unknown) else {
            panic!("an unknown field is a parse error");
        };
        assert!(reason.contains("sede"), "{reason}");
        let no_objects = "objects = []\n".to_owned() + BASIC.split("[[objects]]").next().unwrap();
        assert_eq!(field(&no_objects), "objects");
        assert!(matches!(
            error(&with(BASIC, "count = 1000", "count = \"many\"")),
            ScenarioError::Parse(_)
        ));
    }

    #[test]
    fn rejects_a_hit_ratio_the_popular_objects_cannot_reach() {
        // 100,000 nearly uniform objects after 2,000 warm-up requests.
        let text = with(
            &with(BASIC, "count = 1000", "count = 100000"),
            "zipf = 0.9",
            "zipf = 0.2",
        );
        match error(&text) {
            ScenarioError::Invalid { field, reason } => {
                assert_eq!(field, "keys.hit_ratio");
                assert!(reason.contains("out of reach"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        // A target within the tolerance of what the popular objects reach is
        // accepted, and expected at what they reach.
        let ten = with(BASIC, "count = 1000", "count = 10");
        let s = Scenario::from_toml(&with(&ten, "hit_ratio = 0.8", "hit_ratio = 1.0")).unwrap();
        assert_eq!(s.once_only(), 0.0);
        assert!(s.expected_hit_ratio() > 1.0 - HIT_RATIO_TOLERANCE);
    }

    /// Replays every request of a scenario against a cache that keeps
    /// everything, and returns the measured requests' hit ratio.
    fn simulate(s: &Scenario) -> f64 {
        let requests = s.requests();
        let warmup = s.warmup_requests();
        let total = warmup + s.measured_requests();
        let mut seen = HashSet::new();
        let mut hits = 0u64;
        for seq in 0..total {
            let object = requests.object(seq);
            if seq < warmup {
                assert!(matches!(object, Object::Popular { .. }), "warm-up {seq}");
            }
            let hit = !seen.insert(object.path());
            if seq >= warmup && hit {
                hits += 1;
            }
        }
        hits as f64 / s.measured_requests() as f64
    }

    #[test]
    fn the_requests_reach_the_expected_hit_ratio() {
        for (zipf, count, target) in [
            ("0.9", "1000", "0.8"),
            ("0", "200", "0.95"),
            ("1.2", "50000", "0.6"),
            ("0.6", "2000", "0.3"),
        ] {
            let text = with(
                &with(
                    &with(BASIC, "zipf = 0.9", &format!("zipf = {zipf}")),
                    "count = 1000",
                    &format!("count = {count}"),
                ),
                "hit_ratio = 0.8",
                &format!("hit_ratio = {target}"),
            );
            let s = Scenario::from_toml(&text).unwrap();
            let seen = simulate(&s);
            assert!(
                (seen - s.expected_hit_ratio()).abs() < 0.01,
                "zipf {zipf} count {count}: saw {seen}, expected {}",
                s.expected_hit_ratio()
            );
        }
    }

    #[test]
    fn popularity_follows_the_zipf_law() {
        // 200,000 warm-up requests, all to popular objects.
        let text = with(
            &with(BASIC, "zipf = 0.9", "zipf = 1.0"),
            "rate = 1000",
            "rate = 100000",
        );
        let s = Scenario::from_toml(&text).unwrap();
        let requests = s.requests();
        let n = s.warmup_requests();
        assert_eq!(n, 200_000);
        let mut counts = vec![0u64; 1001];
        for seq in 0..n {
            let Object::Popular { rank, .. } = requests.object(seq) else {
                panic!("warm-up request {seq} is once-only");
            };
            counts[rank as usize] += 1;
        }
        let probabilities = zipf_probabilities(1000, 1.0);
        for rank in [1usize, 2, 10, 100, 1000] {
            let seen = counts[rank] as f64 / n as f64;
            let expected = probabilities[rank - 1];
            // Within five standard deviations of a binomial count.
            let sd = (expected * (1.0 - expected) / n as f64).sqrt();
            assert!(
                (seen - expected).abs() < 5.0 * sd,
                "rank {rank}: {seen} vs {expected}"
            );
        }
    }

    #[test]
    fn each_object_keeps_one_size_drawn_by_weight() {
        let s = Scenario::from_toml(BASIC).unwrap();
        let requests = s.requests();
        let mut sizes = std::collections::HashMap::new();
        let mut small = 0u64;
        let total = s.warmup_requests() + s.measured_requests();
        for seq in 0..total {
            let object = requests.object(seq);
            if object.size() == 1024 {
                small += 1;
            } else {
                assert_eq!(object.size(), 65536);
            }
            if let Object::Popular { rank, size } = object {
                assert_eq!(*sizes.entry(rank).or_insert(size), size, "rank {rank}");
            }
        }
        // By object, 3 in 4 are small; by request, the most popular few
        // weigh in too, so allow a wide margin.
        let share = small as f64 / total as f64;
        assert!((0.55..0.95).contains(&share), "{share}");
        let objects_small = (1..=1000).filter(|&r| s.key_size(r) == 1024).count();
        assert!((680..820).contains(&objects_small), "{objects_small}");
        assert_eq!(
            s.working_set(),
            objects_small as u64 * 1024 + (1000 - objects_small as u64) * 65536
        );
    }

    #[test]
    fn a_large_working_set_is_estimated_from_the_mean() {
        let text = with(
            &with(BASIC, "count = 1000", "count = 1000000"),
            "hit_ratio = 0.8",
            "hit_ratio = 0.01",
        );
        let s = Scenario::from_toml(&text).unwrap();
        let mean = (3.0 * 1024.0 + 65536.0) / 4.0;
        assert_eq!(s.working_set(), (mean * 1e6) as u64);
    }

    #[test]
    fn requests_are_the_same_on_every_run_and_differ_by_seed() {
        let a = Scenario::from_toml(BASIC).unwrap().requests();
        let b = Scenario::from_toml(BASIC).unwrap().requests();
        let c = Scenario::from_toml(&with(BASIC, "seed = 7", "seed = 8"))
            .unwrap()
            .requests();
        let paths = |r: &ScenarioRequests| (0..5000).map(|s| r.object(s)).collect::<Vec<_>>();
        assert_eq!(paths(&a), paths(&b));
        assert_ne!(paths(&a), paths(&c));
        // The request carries the object's path.
        let request = a.request(4321);
        assert_eq!(request.uri().path(), a.object(4321).path());
        assert!(request.uri().path().starts_with("/obj/"));
    }

    #[test]
    fn paths_name_the_size_and_the_object() {
        assert_eq!(Object::Popular { rank: 3, size: 10 }.path(), "/obj/10/k3");
        assert_eq!(Object::OnceOnly { seq: 9, size: 0 }.path(), "/obj/0/u9");
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("10s"), Some(Duration::from_secs(10)));
        assert_eq!(parse_duration("10"), Some(Duration::from_secs(10)));
        assert_eq!(parse_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("2min"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration(" 3 s "), Some(Duration::from_secs(3)));
        for bad in ["", "s", ".5s", "5.s", "1.2.3s", "5d", "-1s", "1e3s"] {
            assert_eq!(parse_duration(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("512B"), Some(512));
        assert_eq!(parse_size("4KiB"), Some(4096));
        assert_eq!(parse_size("1.5MiB"), Some(1_572_864));
        assert_eq!(parse_size("1GiB"), Some(1 << 30));
        assert_eq!(parse_size("2kB"), Some(2000));
        assert_eq!(parse_size("3MB"), Some(3_000_000));
        assert_eq!(parse_size("1GB"), Some(1_000_000_000));
        for bad in [
            "",
            "KiB",
            "1.5",
            "0.1KiB",
            "4kib",
            ".5MiB",
            "1..2B",
            "99999999999GB",
        ] {
            assert_eq!(parse_size(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn grpc_calls_and_an_origin_delay() {
        let s = Scenario::from_toml(BASIC).unwrap();
        assert_eq!((s.kind, s.origin_delay), (RequestKind::Get, Duration::ZERO));
        let grpc = with(
            BASIC,
            "connections = 4",
            "connections = 4\nprotocol = \"http2\"\nrequests = \"grpc\"\norigin_delay = \"200ms\"",
        );
        let s = Scenario::from_toml(&grpc).unwrap();
        assert_eq!(s.kind, RequestKind::Grpc);
        assert_eq!(s.kind.name(), "grpc");
        assert_eq!(s.origin_delay, Duration::from_millis(200));
        // Each request is the unary call for the object a GET would ask for.
        let requests = s.requests();
        let object = requests.object(5000);
        let request = requests.request(5000);
        assert_eq!(request.method(), http::Method::POST);
        assert_eq!(request.uri().path(), origin::GRPC_METHOD);
        assert_eq!(
            request.headers()[http::header::CONTENT_TYPE],
            "application/grpc"
        );
        let body = futures_body(request.into_body());
        assert_eq!(&body[5..], object.path().as_bytes());
        assert_eq!(body[4] as usize, object.path().len());
        let get = Scenario::from_toml(BASIC).unwrap().requests().request(5000);
        assert_eq!(get.method(), http::Method::GET);
        assert_eq!(get.uri().path(), object.path());

        assert_eq!(
            field(&with(BASIC, "seed = 7", "seed = 7\nrequests = \"grpc\"")),
            "requests"
        );
        assert_eq!(
            field(&with(BASIC, "seed = 7", "seed = 7\nrequests = \"put\"")),
            "requests"
        );
        assert_eq!(
            field(&with(BASIC, "seed = 7", "seed = 7\norigin_delay = \"11s\"")),
            "origin_delay"
        );
        assert_eq!(
            field(&with(
                BASIC,
                "seed = 7",
                "seed = 7\norigin_delay = \"soon\""
            )),
            "origin_delay"
        );
        let zero = Scenario::from_toml(&with(BASIC, "seed = 7", "seed = 7\norigin_delay = \"0s\""))
            .unwrap();
        assert_eq!(zero.origin_delay, Duration::ZERO);
    }

    /// The bytes of a request body that is already whole.
    fn futures_body(body: Full<Bytes>) -> Bytes {
        use http_body_util::BodyExt;
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(body.collect())
            .unwrap()
            .to_bytes()
    }

    #[test]
    fn a_file_may_hold_several_scenarios_over_shared_defaults() {
        let text = r#"
            description = "Shared."
            protocol = "http2"
            connections = 10
            origin_delay = "100ms"
            [load]
            rate = 1000
            duration = "5s"
            [keys]
            count = 100
            zipf = 1.0
            hit_ratio = 0.0
            [[objects]]
            size = "1KiB"

            [[scenarios]]
            name = "plain"

            [[scenarios]]
            name = "calls"
            requests = "grpc"
            connections = 4
            [scenarios.load]
            rate = 500
            duration = "2s"
        "#;
        let all = Scenario::all_from_toml(text).unwrap();
        assert_eq!(all.len(), 2);
        let (plain, calls) = (&all[0], &all[1]);
        assert_eq!(plain.name, "plain");
        assert_eq!(plain.kind, RequestKind::Get);
        assert_eq!(plain.connections, 10);
        assert_eq!(plain.measured_requests(), 5000);
        assert_eq!(plain.description, "Shared.");
        assert_eq!(calls.kind, RequestKind::Grpc);
        assert_eq!(calls.connections, 4);
        // A table is replaced whole.
        assert_eq!(calls.measured_requests(), 1000);
        assert_eq!(calls.origin_delay, Duration::from_millis(100));

        // A file of one reads as one.
        assert_eq!(
            Scenario::all_from_toml(BASIC).unwrap(),
            vec![Scenario::from_toml(BASIC).unwrap()]
        );
        // from_toml and load point a file of several at load_all.
        let ScenarioError::Parse(reason) = Scenario::from_toml(text).unwrap_err() else {
            panic!("a file of several is not one scenario");
        };
        assert!(reason.contains("load_all"), "{reason}");

        let dup = text.replace("name = \"calls\"", "name = \"plain\"");
        assert!(matches!(
            Scenario::all_from_toml(&dup).unwrap_err(),
            ScenarioError::Invalid {
                field: "scenarios.name",
                ..
            }
        ));
        assert!(matches!(
            Scenario::all_from_toml(&format!("scenarios = 3\n{BASIC}")).unwrap_err(),
            ScenarioError::Invalid {
                field: "scenarios",
                ..
            }
        ));
        assert!(matches!(
            Scenario::all_from_toml(&format!("scenarios = []\n{BASIC}")).unwrap_err(),
            ScenarioError::Invalid {
                field: "scenarios",
                ..
            }
        ));
        assert!(matches!(
            Scenario::all_from_toml(&format!("scenarios = [1]\n{BASIC}")).unwrap_err(),
            ScenarioError::Invalid {
                field: "scenarios",
                ..
            }
        ));
        // An entry is validated like any scenario.
        let bad = text.replace(
            "requests = \"grpc\"",
            "requests = \"grpc\"\nprotocol = \"http1\"",
        );
        assert!(matches!(
            Scenario::all_from_toml(&bad).unwrap_err(),
            ScenarioError::Invalid {
                field: "requests",
                ..
            }
        ));
        assert!(matches!(
            Scenario::all_from_toml(
                &text.replace("name = \"plain\"", "name = \"plain\"\nbogus = 1")
            )
            .unwrap_err(),
            ScenarioError::Parse(_)
        ));

        let dir = std::env::temp_dir().join(format!("edgefront-several-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("several.toml");
        std::fs::write(&file, text).unwrap();
        assert_eq!(Scenario::load_all(&file).unwrap(), all);
        assert_eq!(Scenario::load_named(&file, "calls").unwrap(), all[1]);
        assert!(matches!(
            Scenario::load_named(&file, "other").unwrap_err(),
            ScenarioError::Invalid { field: "name", .. }
        ));
        std::fs::write(&file, "name = ").unwrap();
        let ScenarioError::Parse(reason) = Scenario::load_all(&file).unwrap_err() else {
            panic!("malformed");
        };
        assert!(reason.contains("several.toml"), "{reason}");
        assert!(matches!(
            Scenario::load_all(dir.join("missing.toml")).unwrap_err(),
            ScenarioError::Io { .. }
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn errors_name_what_is_wrong() {
        let io = ScenarioError::Io {
            path: PathBuf::from("x.toml"),
            reason: "gone".into(),
        };
        assert_eq!(io.to_string(), "x.toml: gone");
        assert_eq!(ScenarioError::Parse("bad".into()).to_string(), "bad");
        assert_eq!(
            invalid("keys.count", "too many").to_string(),
            "keys.count: too many"
        );
    }

    #[test]
    fn loads_files_and_directories() {
        let dir = std::env::temp_dir().join(format!("edgefront-scenarios-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.toml"), BASIC).unwrap();
        std::fs::write(dir.join("a.toml"), with(BASIC, "\"basic\"", "\"first\"")).unwrap();
        std::fs::write(dir.join("notes.txt"), "not a scenario").unwrap();
        let all = Scenario::load_dir(&dir).unwrap();
        let names: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["first", "basic"]);

        std::fs::write(dir.join("c.toml"), "name = ").unwrap();
        let ScenarioError::Parse(reason) = Scenario::load_dir(&dir).unwrap_err() else {
            panic!("a malformed file is a parse error");
        };
        assert!(reason.contains("c.toml"), "{reason}");
        std::fs::write(dir.join("c.toml"), with(BASIC, "count = 1000", "count = 0")).unwrap();
        assert!(matches!(
            Scenario::load_dir(&dir).unwrap_err(),
            ScenarioError::Invalid {
                field: "keys.count",
                ..
            }
        ));

        assert!(matches!(
            Scenario::load(dir.join("missing.toml")),
            Err(ScenarioError::Io { .. })
        ));
        assert!(matches!(
            Scenario::load_dir(dir.join("missing")),
            Err(ScenarioError::Io { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
