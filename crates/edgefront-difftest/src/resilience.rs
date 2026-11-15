//! Diffs edgefront's outlier detection and retry decisions against
//! steer's, decision by decision.
//!
//! Two kinds of trace run through `legacy/steer/cmd/diffdrive` (see
//! `resilience.go` there for the protocol) and through edgefront-proxy:
//!
//! - An [`OutlierTrace`] is one cluster under a failure trace: attempt
//!   outcomes reported against its endpoints, interval ticks, clock moves,
//!   cleared ejections and state reads, under a seeded detector
//!   configuration. steer's `outlier.Detector` answers it on one side and
//!   [`edgefront_proxy::outlier::Detector`] on the other; every ejection
//!   (endpoint, reason and end) and every multiplier must match.
//! - A [`RetryTrace`] is one cluster's request stream: requests with a
//!   method, an `Idempotency-Key` or not, a replayable body or not, and the
//!   outcome of each attempt, through phases of health and outage at
//!   different request rates. On steer's side the decisions come from
//!   `proxy.Idempotent`, `proxy.Retryable` and `proxy.GatewayFailure`
//!   (the code `RoundTrip` runs), `retry.Budget` and `retry.Backoff`; on
//!   ours from [`edgefront_proxy::retry`]. Each attempt's class, whether it
//!   was retried, refused by the budget or out of endpoints, and the
//!   backoff to the nanosecond must match, as must the budget's balance
//!   wherever the trace reads it.
//!
//! Alongside the diff, [`execute_retry`] keeps its own ledger of every
//! request and retry by second and checks each budget decision against
//! it: a retry is granted only while the retries in the window are below
//! `min_per_second × window + ratio × requests`, and refused only when
//! they are not. A run fails on any such breach as it does on a
//! divergence.
//!
//! [`run`] interleaves the two kinds, trace `i` of each generated from seed
//! `seed + i`, and stops at the first line that differs, reporting the
//! seed, the step, the operations just before it and the command that
//! replays it. One retry trace in a hundred (those whose seed is a multiple
//! of 100) is long, by default 100,000 requests.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use edgefront_core::clock::{Clock, ManualClock};
use edgefront_core::config::{self, OutlierDetection};
use edgefront_proxy::balancer::rng::SplitMix64;
use edgefront_proxy::endpoint::Endpoint;
use edgefront_proxy::outlier::{
    Detector, EjectFn, REASON_CONSECUTIVE_GATEWAY_FAILURE, REASON_SUCCESS_RATE,
};
use edgefront_proxy::retry::{self, Budget, Outcome, Policy};
use http::{HeaderMap, HeaderValue, Method};
use serde::{Deserialize, Serialize};

use crate::balancer::Answer;
use crate::rng::Rng;
use crate::steer::Driver;

/// How many operations before a divergence its report shows.
const CONTEXT: usize = 8;

/// The address of endpoint `i` in a trace, as the driver names it.
pub fn host_address(i: usize) -> String {
    format!("10.0.{}.{}:80", i / 250, i % 250 + 1)
}

/// An outlier detector's configuration as the driver takes it: durations
/// in nanoseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetectorSpec {
    pub consecutive_gateway_failures: u32,
    pub interval: u64,
    pub base_ejection_time: u64,
    pub max_ejection_time: u64,
    pub max_ejection_percent: u8,
    pub success_rate_minimum_hosts: u32,
    pub success_rate_request_volume: u32,
    pub success_rate_stdev_factor: f64,
}

impl DetectorSpec {
    /// The configuration edgefront's detector runs with.
    pub fn config(&self) -> OutlierDetection {
        OutlierDetection {
            consecutive_gateway_failures: self.consecutive_gateway_failures,
            interval: Duration::from_nanos(self.interval),
            base_ejection_time: Duration::from_nanos(self.base_ejection_time),
            max_ejection_time: Duration::from_nanos(self.max_ejection_time),
            max_ejection_percent: self.max_ejection_percent,
            success_rate_minimum_hosts: self.success_rate_minimum_hosts,
            success_rate_request_volume: self.success_rate_request_volume,
            success_rate_stdev_factor: self.success_rate_stdev_factor,
        }
    }
}

/// One operation of an outlier trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutlierOp {
    /// An attempt against `ep` ended; the flags say how.
    Report {
        ep: usize,
        gateway: bool,
        failure: bool,
    },
    /// The detector's per-interval work.
    Tick,
    /// The clock moves forward.
    Advance { nanos: u64 },
    /// An endpoint's ejection ends and its multiplier is forgotten.
    Clear { ep: usize },
    /// Every endpoint's multiplier and ejection end.
    State,
}

impl fmt::Display for OutlierOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            OutlierOp::Report {
                ep,
                gateway,
                failure,
            } => write!(f, "f {ep} {} {}", u8::from(gateway), u8::from(failure)),
            OutlierOp::Tick => f.write_str("k"),
            OutlierOp::Advance { nanos } => write!(f, "t {nanos}"),
            OutlierOp::Clear { ep } => write!(f, "c {ep}"),
            OutlierOp::State => f.write_str("s"),
        }
    }
}

/// Go's `strconv.ParseInt` as a non-negative number: an optional sign and
/// decimal digits, within int64.
fn go_uint(s: &str) -> Option<u64> {
    s.parse::<i64>().ok().and_then(|v| u64::try_from(v).ok())
}

impl OutlierOp {
    /// Parses one script line with the driver's rules.
    pub fn parse(line: &str, hosts: usize) -> Result<OutlierOp, String> {
        let f: Vec<&str> = line.split_whitespace().collect();
        let ep = |s: &str| {
            go_uint(s)
                .and_then(|v| usize::try_from(v).ok())
                .filter(|&v| v < hosts)
        };
        let flag = |s: &str| match go_uint(s) {
            Some(0) => Some(false),
            Some(1) => Some(true),
            _ => None,
        };
        let op = match f.as_slice() {
            ["f", e, g, x] => match (ep(e), flag(g), flag(x)) {
                (Some(ep), Some(gateway), Some(failure)) => Some(OutlierOp::Report {
                    ep,
                    gateway,
                    failure,
                }),
                _ => None,
            },
            ["k"] => Some(OutlierOp::Tick),
            ["t", n] => go_uint(n).map(|nanos| OutlierOp::Advance { nanos }),
            ["c", e] => ep(e).map(|ep| OutlierOp::Clear { ep }),
            ["s"] => Some(OutlierOp::State),
            _ => None,
        };
        op.ok_or_else(|| format!("{line:?}: malformed"))
    }
}

/// One outlier trace.
#[derive(Debug, Clone, PartialEq)]
pub struct OutlierTrace {
    pub hosts: usize,
    pub detector: DetectorSpec,
    pub ops: Vec<OutlierOp>,
}

#[derive(Serialize, Deserialize)]
struct WireOutlier {
    op: String,
    hosts: usize,
    outlier: DetectorSpec,
    script: String,
}

/// An attempt's outcome in a retry trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    /// A response below 500.
    Success,
    /// A response from 500 to 599.
    Status(u16),
    Connect,
    Timeout,
    Reset,
}

impl Attempt {
    /// How edgefront classifies it.
    pub fn outcome(self) -> Outcome {
        match self {
            Attempt::Success => Outcome::Success,
            Attempt::Status(s) => Outcome::of_status(s),
            Attempt::Connect => Outcome::ConnectError,
            Attempt::Timeout => Outcome::Timeout,
            Attempt::Reset => Outcome::Reset,
        }
    }
}

impl fmt::Display for Attempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Attempt::Success => f.write_str("s"),
            Attempt::Status(s) => write!(f, "{s}"),
            Attempt::Connect => f.write_str("c"),
            Attempt::Timeout => f.write_str("t"),
            Attempt::Reset => f.write_str("r"),
        }
    }
}

/// One operation of a retry trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryOp {
    /// A request and the outcomes of its attempts, in order; attempts
    /// past the list succeed.
    Request {
        method: String,
        key: bool,
        replayable: bool,
        attempts: Vec<Attempt>,
    },
    /// The clock moves forward.
    Advance { nanos: u64 },
    /// The budget's balance.
    Balance,
}

impl fmt::Display for RetryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RetryOp::Request {
                method,
                key,
                replayable,
                attempts,
            } => {
                write!(f, "q {method} {} {}", u8::from(*key), u8::from(*replayable))?;
                for a in attempts {
                    write!(f, " {a}")?;
                }
                Ok(())
            }
            RetryOp::Advance { nanos } => write!(f, "t {nanos}"),
            RetryOp::Balance => f.write_str("b"),
        }
    }
}

impl RetryOp {
    /// Parses one script line with the driver's rules.
    pub fn parse(line: &str) -> Result<RetryOp, String> {
        let f: Vec<&str> = line.split_whitespace().collect();
        let bad = || format!("{line:?}: malformed");
        let bit = |s: &str| match s {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        };
        match f.as_slice() {
            ["q", method, key, replayable, outs @ ..] => {
                let (Some(key), Some(replayable)) = (bit(key), bit(replayable)) else {
                    return Err(bad());
                };
                Method::from_bytes(method.as_bytes()).map_err(|_| bad())?;
                let attempts = outs
                    .iter()
                    .map(|o| match *o {
                        "s" => Some(Attempt::Success),
                        "c" => Some(Attempt::Connect),
                        "t" => Some(Attempt::Timeout),
                        "r" => Some(Attempt::Reset),
                        // strconv.Atoi, then the 5xx range.
                        s => s
                            .parse::<i64>()
                            .ok()
                            .filter(|c| (500..=599).contains(c))
                            .map(|c| Attempt::Status(c as u16)),
                    })
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(bad)?;
                Ok(RetryOp::Request {
                    method: (*method).to_string(),
                    key,
                    replayable,
                    attempts,
                })
            }
            ["t", n] => go_uint(n)
                .map(|nanos| RetryOp::Advance { nanos })
                .ok_or_else(bad),
            ["b"] => Ok(RetryOp::Balance),
            _ => Err(bad()),
        }
    }
}

/// A cluster's retry settings as the driver takes them: durations in
/// nanoseconds, the budget's fields inline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrySpec {
    pub max_retries: u32,
    pub retry_on: Vec<u16>,
    pub backoff_base: u64,
    pub backoff_max: u64,
    pub ratio: f64,
    pub min_per_second: u32,
    pub window: u64,
}

impl RetrySpec {
    /// The configuration edgefront's retry code runs with.
    pub fn config(&self) -> config::Retry {
        config::Retry {
            max_retries: self.max_retries,
            retry_on: self.retry_on.clone(),
            per_try_timeout: Duration::ZERO,
            backoff_base: Duration::from_nanos(self.backoff_base),
            backoff_max: Duration::from_nanos(self.backoff_max),
            max_body_bytes: 0,
            budget: config::Budget {
                ratio: self.ratio,
                min_per_second: self.min_per_second,
                window: Duration::from_nanos(self.window),
            },
        }
    }
}

/// One retry trace.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryTrace {
    pub hosts: usize,
    /// The seed of the SplitMix64 the backoff jitter is drawn from.
    pub seed: u64,
    pub retry: RetrySpec,
    pub ops: Vec<RetryOp>,
}

#[derive(Serialize, Deserialize)]
struct WireRetry {
    op: String,
    hosts: usize,
    seed: u64,
    retry: RetrySpec,
    script: String,
}

/// Either kind of trace.
#[derive(Debug, Clone, PartialEq)]
pub enum Trace {
    Outlier(OutlierTrace),
    Retry(RetryTrace),
}

fn script<T: fmt::Display>(ops: &[T]) -> String {
    let mut s = String::with_capacity(ops.len() * 12);
    for (i, op) in ops.iter().enumerate() {
        if i > 0 {
            s.push('\n');
        }
        s.push_str(&op.to_string());
    }
    s
}

impl Trace {
    /// The kind, as the driver's op name.
    pub fn kind(&self) -> &'static str {
        match self {
            Trace::Outlier(_) => "outlier",
            Trace::Retry(_) => "retry",
        }
    }

    /// The number of operations.
    pub fn len(&self) -> usize {
        match self {
            Trace::Outlier(t) => t.ops.len(),
            Trace::Retry(t) => t.ops.len(),
        }
    }

    /// Whether the script is empty, which no driver accepts.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Operation `k` as a script line.
    pub fn op(&self, k: usize) -> Option<String> {
        match self {
            Trace::Outlier(t) => t.ops.get(k).map(ToString::to_string),
            Trace::Retry(t) => t.ops.get(k).map(ToString::to_string),
        }
    }

    /// The trace as one query line, without the newline.
    pub fn to_line(&self) -> String {
        let line = match self {
            Trace::Outlier(t) => serde_json::to_string(&WireOutlier {
                op: "outlier".into(),
                hosts: t.hosts,
                outlier: t.detector.clone(),
                script: script(&t.ops),
            }),
            Trace::Retry(t) => serde_json::to_string(&WireRetry {
                op: "retry".into(),
                hosts: t.hosts,
                seed: t.seed,
                retry: t.retry.clone(),
                script: script(&t.ops),
            }),
        };
        line.expect("traces always serialise")
    }

    /// Parses a line written by [`Trace::to_line`].
    pub fn from_line(line: &str) -> Result<Trace, String> {
        #[derive(Deserialize)]
        struct Op {
            op: String,
        }
        let op: Op = serde_json::from_str(line).map_err(|e| e.to_string())?;
        fn ops<T>(
            script: &str,
            parse: impl Fn(&str) -> Result<T, String>,
        ) -> Result<Vec<T>, String> {
            script
                .split('\n')
                .enumerate()
                .map(|(i, l)| parse(l).map_err(|e| format!("op {}: {e}", i + 1)))
                .collect()
        }
        match op.op.as_str() {
            "outlier" => {
                let w: WireOutlier = serde_json::from_str(line).map_err(|e| e.to_string())?;
                let d = &w.outlier;
                if w.hosts == 0
                    || d.base_ejection_time == 0
                    || d.max_ejection_time < d.base_ejection_time
                {
                    return Err("outlier query needs hosts and a valid detector".into());
                }
                let parsed = ops(&w.script, |l| OutlierOp::parse(l, w.hosts))?;
                Ok(Trace::Outlier(OutlierTrace {
                    hosts: w.hosts,
                    detector: w.outlier,
                    ops: parsed,
                }))
            }
            "retry" => {
                let w: WireRetry = serde_json::from_str(line).map_err(|e| e.to_string())?;
                if w.hosts == 0 || w.retry.window < SECOND {
                    return Err("retry query needs hosts and a valid policy".into());
                }
                if let Some(s) = w.retry.retry_on.iter().find(|s| !(500..=599).contains(*s)) {
                    return Err(format!("retry_on {s} is not a 5xx status"));
                }
                let parsed = ops(&w.script, RetryOp::parse)?;
                Ok(Trace::Retry(RetryTrace {
                    hosts: w.hosts,
                    seed: w.seed,
                    retry: w.retry,
                    ops: parsed,
                }))
            }
            other => Err(format!("unknown op {other:?}")),
        }
    }
}

/// What an outlier trace exercised, as edgefront's detector saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutlierStats {
    pub reports: usize,
    pub ticks: usize,
    pub consecutive_ejections: usize,
    pub success_rate_ejections: usize,
    /// Ejections of an endpoint already ejected more than once, so with a
    /// longer ejection time.
    pub repeat_ejections: usize,
    /// Ejections that ran into `max_ejection_time`.
    pub capped_ejections: usize,
    pub states: usize,
}

impl OutlierStats {
    fn add(&mut self, o: &OutlierStats) {
        self.reports += o.reports;
        self.ticks += o.ticks;
        self.consecutive_ejections += o.consecutive_ejections;
        self.success_rate_ejections += o.success_rate_ejections;
        self.repeat_ejections += o.repeat_ejections;
        self.capped_ejections += o.capped_ejections;
        self.states += o.states;
    }
}

/// An ejection the callback saw: the endpoint, the reason, when it ends in
/// nanoseconds from the start, and the endpoint's multiplier.
type Ejection = (usize, String, u64, u64);

/// Runs an outlier trace through edgefront's detector.
pub fn execute_outlier(t: &OutlierTrace) -> (Answer, OutlierStats) {
    let clock = Arc::new(ManualClock::new());
    let epoch = clock.start();
    let eps: Vec<Endpoint> = (0..t.hosts)
        .map(|i| Endpoint::new(&host_address(i), 1, 0, epoch))
        .collect();
    let index: BTreeMap<Box<str>, usize> = eps
        .iter()
        .enumerate()
        .map(|(i, e)| (e.address.clone(), i))
        .collect();
    let ejections: Arc<Mutex<Vec<Ejection>>> = Arc::default();
    let sink = Arc::clone(&ejections);
    let on_eject: EjectFn = Box::new(move |ep, reason, until| {
        let i = index[&ep.address];
        let nanos = until.duration_since(epoch).as_nanos() as u64;
        sink.lock().unwrap_or_else(PoisonError::into_inner).push((
            i,
            reason.to_string(),
            nanos,
            ep.state.times_ejected(),
        ));
    });
    let detector = Detector::new(
        t.detector.config(),
        eps,
        Arc::clone(&clock) as Arc<dyn Clock>,
        Some(on_eject),
    );
    let max = t.detector.max_ejection_time;
    let mut stats = OutlierStats::default();
    let mut lines = Vec::with_capacity(t.ops.len());
    let take = |stats: &mut OutlierStats| -> String {
        let mut taken = ejections.lock().unwrap_or_else(PoisonError::into_inner);
        if taken.is_empty() {
            return "ok".into();
        }
        let now = clock.elapsed().as_nanos() as u64;
        let line = taken
            .iter()
            .map(|(i, reason, until, times)| {
                if reason == REASON_SUCCESS_RATE {
                    stats.success_rate_ejections += 1;
                } else if reason == REASON_CONSECUTIVE_GATEWAY_FAILURE {
                    stats.consecutive_ejections += 1;
                }
                if *times > 1 {
                    stats.repeat_ejections += 1;
                }
                if until - now == max && *times > 1 {
                    stats.capped_ejections += 1;
                }
                format!("eject {i} {reason} {until}")
            })
            .collect::<Vec<_>>()
            .join("; ");
        taken.clear();
        line
    };
    for op in &t.ops {
        let line = match *op {
            OutlierOp::Report {
                ep,
                gateway,
                failure,
            } => {
                stats.reports += 1;
                detector.report(&detector.endpoints()[ep], gateway, failure);
                take(&mut stats)
            }
            OutlierOp::Tick => {
                stats.ticks += 1;
                detector.tick();
                take(&mut stats)
            }
            OutlierOp::Advance { nanos } => {
                clock.advance(Duration::from_nanos(nanos));
                "ok".into()
            }
            OutlierOp::Clear { ep } => {
                detector.endpoints()[ep].state.clear_ejection();
                "ok".into()
            }
            OutlierOp::State => {
                stats.states += 1;
                let mut s = String::from("state");
                for e in detector.endpoints() {
                    let until = e.state.ejected_until().map_or("-".to_string(), |u| {
                        u.duration_since(epoch).as_nanos().to_string()
                    });
                    s.push_str(&format!(" {}/{until}", e.state.times_ejected()));
                }
                s
            }
        };
        lines.push(line);
    }
    let answer = Answer {
        ok: true,
        lines,
        ..Answer::default()
    };
    (answer, stats)
}

/// What a retry trace exercised, and what the ledger found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetryStats {
    pub requests: usize,
    pub attempts: usize,
    pub retries: usize,
    /// Retries the budget refused.
    pub exhausted: usize,
    /// Retries with no endpoint left to try.
    pub no_endpoint: usize,
    /// Budget reads.
    pub balances: usize,
    /// Retries granted while the ledger's window was already full.
    pub overdrawn: usize,
    /// Retries refused while the ledger's window had room.
    pub refused_with_room: usize,
}

impl RetryStats {
    fn add(&mut self, o: &RetryStats) {
        self.requests += o.requests;
        self.attempts += o.attempts;
        self.retries += o.retries;
        self.exhausted += o.exhausted;
        self.no_endpoint += o.no_endpoint;
        self.balances += o.balances;
        self.overdrawn += o.overdrawn;
        self.refused_with_room += o.refused_with_room;
    }
}

/// An independent count of requests and retries by second, to check the
/// budget's decisions against.
struct Ledger {
    window: u64,
    floor: f64,
    ratio: f64,
    /// (second, requests, retries), oldest first, within the window.
    seconds: VecDeque<(u64, u64, u64)>,
}

impl Ledger {
    fn new(spec: &RetrySpec) -> Ledger {
        let window = (spec.window / 1_000_000_000).max(1);
        Ledger {
            window,
            floor: (u64::from(spec.min_per_second) * window) as f64,
            ratio: spec.ratio,
            seconds: VecDeque::new(),
        }
    }

    fn at(&mut self, sec: u64) -> &mut (u64, u64, u64) {
        while self
            .seconds
            .front()
            .is_some_and(|&(s, _, _)| s + self.window <= sec)
        {
            self.seconds.pop_front();
        }
        if self.seconds.back().is_none_or(|&(s, _, _)| s != sec) {
            self.seconds.push_back((sec, 0, 0));
        }
        self.seconds.back_mut().expect("just pushed")
    }

    /// Whether a retry at `sec` fits: the window's retries are below the
    /// floor plus the ratio of its requests.
    fn room(&mut self, sec: u64) -> bool {
        self.at(sec);
        let (requests, retries) = self
            .seconds
            .iter()
            .fold((0, 0), |(q, r), &(_, sq, sr)| (q + sq, r + sr));
        (retries as f64) < self.floor + self.ratio * requests as f64
    }
}

/// Runs a retry trace through edgefront's retry code.
pub fn execute_retry(t: &RetryTrace) -> (Answer, RetryStats) {
    let clock = Arc::new(ManualClock::new());
    let cfg = t.retry.config();
    let policy = Policy::new(&cfg);
    let budget = Budget::with_origin(
        &cfg.budget,
        Arc::clone(&clock) as Arc<dyn Clock>,
        clock.start(),
    );
    let jitter = SplitMix64::new(t.seed);
    let mut ledger = Ledger::new(&t.retry);
    let mut stats = RetryStats::default();
    let mut lines = Vec::with_capacity(t.ops.len());
    for op in &t.ops {
        let line = match op {
            RetryOp::Request {
                method,
                key,
                replayable,
                attempts,
            } => {
                stats.requests += 1;
                let method =
                    Method::from_bytes(method.as_bytes()).expect("parsed methods are valid");
                let mut headers = HeaderMap::new();
                if *key {
                    headers.insert("idempotency-key", HeaderValue::from_static("k"));
                }
                budget.deposit();
                let sec = clock.elapsed().as_secs();
                ledger.at(sec).1 += 1;
                let safe = retry::idempotent(&method, &headers);
                let mut line = String::new();
                for attempt in 0u32.. {
                    stats.attempts += 1;
                    let outcome = attempts
                        .get(attempt as usize)
                        .map_or(Outcome::Success, |a| a.outcome());
                    if attempt > 0 {
                        line.push(',');
                    }
                    line.push_str(if outcome.is_gateway_failure() {
                        "g "
                    } else if outcome.is_failure() {
                        "f "
                    } else {
                        "o "
                    });
                    if !policy.retryable(attempt, *replayable, safe, outcome) {
                        line.push('d');
                        break;
                    }
                    if attempt as usize + 1 >= t.hosts {
                        stats.no_endpoint += 1;
                        line.push('n');
                        break;
                    }
                    let room = ledger.room(sec);
                    if !budget.try_withdraw() {
                        stats.exhausted += 1;
                        if room {
                            stats.refused_with_room += 1;
                        }
                        line.push('x');
                        break;
                    }
                    if !room {
                        stats.overdrawn += 1;
                    }
                    ledger.at(sec).2 += 1;
                    stats.retries += 1;
                    let random = (jitter.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
                    let wait = policy.backoff(attempt, random);
                    line.push_str(&format!("r{}", wait.as_nanos()));
                }
                line
            }
            RetryOp::Advance { nanos } => {
                clock.advance(Duration::from_nanos(*nanos));
                "ok".into()
            }
            RetryOp::Balance => {
                stats.balances += 1;
                format!("balance {}", budget.balance())
            }
        };
        lines.push(line);
    }
    let answer = Answer {
        ok: true,
        lines,
        ..Answer::default()
    };
    (answer, stats)
}

/// Answers a trace from edgefront's crates.
pub fn answer(t: &Trace) -> Answer {
    match t {
        Trace::Outlier(t) => execute_outlier(t).0,
        Trace::Retry(t) => execute_retry(t).0,
    }
}

/// Answers an `outlier` or `retry` query line from edgefront's crates, for
/// the Rust twin of the driver. A malformed line is an error, as it is for
/// steer's.
pub fn answer_line(line: &str) -> Result<Answer, String> {
    Trace::from_line(line).map(|t| answer(&t))
}

/// Something that runs traces: steer's driver, or a stand-in in tests.
pub trait Oracle {
    fn ask(&mut self, t: &Trace) -> io::Result<Answer>;
}

impl Oracle for Driver {
    fn ask(&mut self, t: &Trace) -> io::Result<Answer> {
        let line = self.ask_line(&t.to_line())?;
        serde_json::from_str(&line).map_err(|e| {
            let shown: String = line.trim_end().chars().take(400).collect();
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad answer {shown:?}: {e}"),
            )
        })
    }
}

/// How much to generate, and from which seed. Trace `i` of each kind is
/// generated from seed `seed + i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// Traces of each kind.
    pub traces: usize,
    /// Operations per outlier trace.
    pub steps: usize,
    /// Requests per retry trace.
    pub requests: usize,
    /// Requests per long retry trace, one whose seed is a multiple of 100.
    pub long: usize,
    pub seed: u64,
}

impl Plan {
    /// The requests of the retry trace from `seed`.
    pub fn requests_for(&self, seed: u64) -> usize {
        if seed.is_multiple_of(100) {
            self.long
        } else {
            self.requests
        }
    }
}

/// The first place the implementations disagreed, or where the ledger
/// caught the budget out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    pub kind: &'static str,
    /// Which trace of the run, from 0.
    pub trace: usize,
    /// The seed that generates it.
    pub seed: u64,
    /// The 1-based operation, or 0 for the trace as a whole.
    pub step: usize,
    pub detail: String,
}

/// What a run compared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub plan: Option<Plan>,
    pub outlier_traces: usize,
    pub retry_traces: usize,
    pub long_traces: usize,
    pub outlier: OutlierStats,
    pub retry: RetryStats,
    pub divergence: Option<Divergence>,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let plan = self.plan.unwrap_or(Plan {
            traces: 0,
            steps: 0,
            requests: 0,
            long: 0,
            seed: 0,
        });
        let o = &self.outlier;
        let r = &self.retry;
        writeln!(f, "seed {}", plan.seed)?;
        writeln!(
            f,
            "outlier traces: {} ({} reports, {} ticks, {} state reads)",
            self.outlier_traces, o.reports, o.ticks, o.states
        )?;
        writeln!(
            f,
            "ejections: {} consecutive_gateway_failure, {} success_rate ({} repeat, {} at max_ejection_time)",
            o.consecutive_ejections,
            o.success_rate_ejections,
            o.repeat_ejections,
            o.capped_ejections
        )?;
        writeln!(
            f,
            "retry traces: {} ({} of {} requests)",
            self.retry_traces, self.long_traces, plan.long
        )?;
        writeln!(
            f,
            "requests: {} ({} attempts; {} retries, {} refused by the budget, {} out of endpoints; {} balance reads)",
            r.requests, r.attempts, r.retries, r.exhausted, r.no_endpoint, r.balances
        )?;
        writeln!(
            f,
            "budget ledger: {} retries over budget, {} refused with room",
            r.overdrawn, r.refused_with_room
        )?;
        match &self.divergence {
            None => write!(f, "divergences: 0"),
            Some(d) => write!(
                f,
                "divergences: 1 (stopped at the first)\n\n{} trace {} (seed {}) step {}\n{}\nreplay: edgefront-difftest resilience --traces 1 --steps {} --requests {} --long {} --seed {} -- DRIVER",
                d.kind,
                d.trace,
                d.seed,
                d.step,
                d.detail,
                plan.steps,
                plan.requests,
                plan.long,
                d.seed
            ),
        }
    }
}

/// Generates `plan.traces` traces of each kind, runs each through `oracle`
/// and edgefront's crates, and stops at the first line that differs or the
/// first budget decision the ledger disputes.
pub fn run(oracle: &mut dyn Oracle, plan: Plan) -> io::Result<Report> {
    let mut report = Report {
        plan: Some(plan),
        ..Report::default()
    };
    for i in 0..plan.traces {
        let seed = plan.seed.wrapping_add(i as u64);
        let trace = Trace::Outlier(generate_outlier(seed, plan.steps));
        let theirs = oracle.ask(&trace)?;
        let Trace::Outlier(t) = &trace else {
            unreachable!()
        };
        let (ours, stats) = execute_outlier(t);
        report.outlier_traces += 1;
        if let Some((step, detail)) = compare(&trace, &theirs, &ours) {
            report.divergence = Some(Divergence {
                kind: "outlier",
                trace: i,
                seed,
                step,
                detail,
            });
            return Ok(report);
        }
        report.outlier.add(&stats);

        let t = generate_retry(seed, plan.requests_for(seed));
        let trace = Trace::Retry(t);
        let theirs = oracle.ask(&trace)?;
        let Trace::Retry(t) = &trace else {
            unreachable!()
        };
        let (ours, stats) = execute_retry(t);
        report.retry_traces += 1;
        if seed.is_multiple_of(100) {
            report.long_traces += 1;
        }
        let found = compare(&trace, &theirs, &ours).or_else(|| {
            (stats.overdrawn + stats.refused_with_room > 0).then(|| {
                (
                    0,
                    format!(
                        "{}\nthe budget's decisions disagree with the ledger: {} retries over budget, {} refused with room",
                        describe(&trace),
                        stats.overdrawn,
                        stats.refused_with_room
                    ),
                )
            })
        });
        if let Some((step, detail)) = found {
            report.divergence = Some(Divergence {
                kind: "retry",
                trace: i,
                seed,
                step,
                detail,
            });
            return Ok(report);
        }
        report.retry.add(&stats);
    }
    Ok(report)
}

fn describe(t: &Trace) -> String {
    match t {
        Trace::Outlier(t) => format!("hosts {} detector {:?}", t.hosts, t.detector),
        Trace::Retry(t) => format!(
            "hosts {} jitter seed {} retry {:?}",
            t.hosts, t.seed, t.retry
        ),
    }
}

/// The first difference between two answers: its 1-based step (0 for the
/// trace itself) and a description.
fn compare(t: &Trace, theirs: &Answer, ours: &Answer) -> Option<(usize, String)> {
    let head = describe(t);
    if theirs.ok != ours.ok || theirs.class != ours.class || theirs.error != ours.error {
        let brief = |a: &Answer| format!("ok={} class={:?} error={:?}", a.ok, a.class, a.error);
        return Some((
            0,
            format!(
                "{head}\nsteer: {}\nedgefront: {}",
                brief(theirs),
                brief(ours)
            ),
        ));
    }
    let n = theirs.lines.len().max(ours.lines.len());
    let k = (0..n).find(|&k| theirs.lines.get(k) != ours.lines.get(k))?;
    let mut before = String::new();
    for j in k.saturating_sub(CONTEXT)..k {
        before.push_str(&format!(
            "\n  {:>6}  {:<28} {}",
            j + 1,
            t.op(j).unwrap_or_default(),
            ours.lines[j]
        ));
    }
    let show = |l: Option<&String>| l.map_or("(no line)".to_string(), Clone::clone);
    let op = t
        .op(k)
        .unwrap_or_else(|| "(past the end of the script)".to_string());
    Some((
        k + 1,
        format!(
            "{head}\nbefore:{before}\nop: {op}\nsteer: {}\nedgefront: {}",
            show(theirs.lines.get(k)),
            show(ours.lines.get(k))
        ),
    ))
}

const SECOND: u64 = 1_000_000_000;
const MILLI: u64 = 1_000_000;

/// The outlier trace `seed` generates, with `steps` operations.
///
/// The detector's settings are drawn across the ranges config validation
/// allows, biased towards the values that make both detectors fire. Each
/// endpoint has a failure profile that changes between phases: healthy,
/// flaky, failing with 5xx, or failing at the gateway, so runs of gateway
/// failures and success-rate outliers both happen. Reports arrive at a
/// rate per interval, the clock moves between them, and a tick follows
/// each interval, with stray ticks, long pauses that let ejections end
/// and multipliers decay, state reads and cleared ejections in between.
pub fn generate_outlier(seed: u64, steps: usize) -> OutlierTrace {
    let mut g = Rng::new(seed ^ 0x6f75_746c_6965_7200);
    let hosts = match g.below(100) {
        0..75 => g.range(1, 12),
        75..95 => g.range(13, 40),
        _ => g.range(41, 120),
    };
    let interval = g.pick(&[10 * MILLI, 100 * MILLI, SECOND, SECOND, 10 * SECOND]);
    let base = match g.below(6) {
        0 => MILLI,
        1 => g.range(1, 1_000) as u64 * MILLI,
        2 => interval,
        _ => g.pick(&[SECOND, 5 * SECOND, 30 * SECOND]),
    };
    let max_ejection_time = match g.below(5) {
        0 => base,
        1 => base * g.range(2, 4) as u64 + g.below(1_000) as u64 * MILLI,
        _ => (base * g.range(2, 20) as u64).min(86_400 * SECOND),
    };
    let detector = DetectorSpec {
        consecutive_gateway_failures: if g.chance(0.1) {
            0
        } else {
            g.pick(&[1, 2, 3, 5, 5, 5, 7, 10, 20]) as u32
        },
        interval,
        base_ejection_time: base,
        max_ejection_time,
        max_ejection_percent: match g.below(8) {
            0 => 0,
            1 => 100,
            2 => g.range(1, 99) as u8,
            3 => 50,
            _ => g.pick(&[10, 20, 25, 34]),
        },
        success_rate_minimum_hosts: if g.chance(0.15) {
            0
        } else {
            g.range(1, hosts.clamp(1, 8) + 1) as u32
        },
        success_rate_request_volume: match g.below(6) {
            0 => 1,
            1 => g.range(2, 5) as u32,
            2 => g.range(50, 200) as u32,
            _ => g.range(5, 30) as u32,
        },
        success_rate_stdev_factor: match g.below(6) {
            0 => 1.0,
            1 => 0.5,
            2 => 10.0,
            3 => (g.range(1, 1_000) as f64) / 100.0,
            _ => 1.9,
        },
    };
    // Failure probabilities: (gateway, other 5xx).
    let profiles = [
        (0.0, 0.0),
        (0.0, 0.01),
        (0.02, 0.02),
        (0.1, 0.1),
        (0.0, 0.6),
        (0.7, 0.1),
        (1.0, 0.0),
    ];
    let mut profile: Vec<(f64, f64)> = (0..hosts).map(|_| g.pick(&profiles[..4])).collect();
    let mut per_interval = g.range(5, 400);
    let mut until_tick = interval;
    let mut ops = Vec::with_capacity(steps);
    while ops.len() < steps {
        match g.below(1_000) {
            // A new phase: some endpoints start or stop failing.
            0..5 => {
                for p in profile.iter_mut() {
                    if g.chance(0.3) {
                        *p = g.pick(&profiles);
                    }
                }
                per_interval = g.range(5, 400);
            }
            // A pause: ejections end, multipliers decay.
            5..12 => {
                let nanos = g.range(1, 40) as u64 * base / 4 + g.below(1_000) as u64;
                ops.push(OutlierOp::Advance { nanos });
                until_tick = 0;
            }
            12..20 => ops.push(OutlierOp::State),
            20..22 => ops.push(OutlierOp::Clear { ep: g.below(hosts) }),
            22..30 => ops.push(OutlierOp::Tick),
            _ => {
                let ep = g.below(hosts);
                let (gw, other) = profile[ep];
                let roll = (g.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
                let (gateway, failure) = if roll < gw {
                    // steer only ever reports a gateway failure as a failure
                    // too; the odd one that is not checks the counters.
                    (true, !g.chance(0.01))
                } else if roll < gw + other {
                    (false, true)
                } else {
                    (false, false)
                };
                ops.push(OutlierOp::Report {
                    ep,
                    gateway,
                    failure,
                });
                let step = interval / per_interval as u64;
                let nanos = if g.chance(0.2) {
                    0
                } else {
                    g.range(0, 2 * step as usize) as u64
                };
                if nanos > 0 {
                    ops.push(OutlierOp::Advance { nanos });
                }
                until_tick = until_tick.saturating_sub(nanos);
            }
        }
        if until_tick == 0 {
            ops.push(OutlierOp::Tick);
            until_tick = interval;
        }
    }
    ops.truncate(steps);
    if ops.is_empty() {
        ops.push(OutlierOp::State);
    }
    OutlierTrace {
        hosts,
        detector,
        ops,
    }
}

const METHODS: [&str; 12] = [
    "GET", "GET", "GET", "GET", "POST", "POST", "PUT", "DELETE", "HEAD", "OPTIONS", "PATCH",
    "TRACE",
];

/// The retry trace `seed` generates, with `requests` requests.
///
/// The policy is drawn across the ranges config validation allows. Traffic
/// comes in phases with their own request rate (one a second up to tens
/// of thousands) and failure rate (none, a little, half, everything), so
/// the budget fills, runs dry and refills as its window slides. Requests
/// mix methods, idempotency keys and unreplayable bodies, and attempts
/// fail with every kind of outcome, including 5xx statuses on both sides
/// of `retry_on`.
pub fn generate_retry(seed: u64, requests: usize) -> RetryTrace {
    let mut g = Rng::new(seed ^ 0x7265_7472_7900_0000);
    let hosts = g.pick(&[1, 2, 2, 3, 3, 3, 4, 6]);
    let max_retries = if g.chance(0.08) {
        0
    } else {
        g.range(1, 5) as u32
    };
    let retry_on: Vec<u16> = match g.below(5) {
        0 => Vec::new(),
        1 => (500..=599).filter(|_| g.chance(0.2)).collect(),
        2 => vec![500, 502, 503, 504],
        _ => vec![502, 503, 504],
    };
    let backoff_base = g.pick(&[
        MILLI,
        10 * MILLI,
        25 * MILLI,
        25 * MILLI,
        100 * MILLI,
        SECOND,
    ]);
    let backoff_max = match g.below(4) {
        0 => backoff_base,
        _ => (backoff_base * g.range(2, 100) as u64).min(60 * SECOND),
    };
    let ratio = match g.below(7) {
        0 => 0.0,
        1 => 1.0,
        2 => 0.1,
        3 => (g.range(1, 999) as f64) / 1_000.0,
        _ => 0.2,
    };
    let min_per_second = match g.below(8) {
        0 => 0,
        1 => 1,
        2 => g.range(100, 1_000) as u32,
        3 if g.chance(0.1) => 100_000,
        _ => g.pick(&[2, 5, 10, 10]) as u32,
    };
    let window = g.pick(&[1, 2, 5, 10, 10, 30, 60]) as u64 * SECOND;
    let retry = RetrySpec {
        max_retries,
        retry_on,
        backoff_base,
        backoff_max,
        ratio,
        min_per_second,
        window,
    };
    let statuses: [u16; 9] = [500, 501, 502, 503, 503, 504, 505, 529, 599];
    let mut rate = 0u64;
    let mut failing = 0.0;
    let mut phase_left = 0;
    let mut ops = Vec::with_capacity(requests * 2);
    let mut made = 0;
    while made < requests {
        if phase_left == 0 {
            phase_left = g.range(50, 5_000);
            rate = g.pick(&[1, 10, 100, 1_000, 1_000, 10_000, 50_000]);
            failing = g.pick(&[0.0, 0.0, 0.02, 0.2, 0.5, 0.9, 1.0]);
        }
        phase_left -= 1;
        // The gap to the next request: a uniform draw around 1 / rate.
        let mean = SECOND / rate;
        let gap = g.range(0, (2 * mean) as usize) as u64;
        if gap > 0 {
            ops.push(RetryOp::Advance { nanos: gap });
        }
        if g.chance(0.01) {
            ops.push(RetryOp::Balance);
        }
        let mut attempts = Vec::new();
        for _ in 0..=max_retries {
            let roll = (g.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
            if roll >= failing {
                if g.chance(0.5) {
                    attempts.push(Attempt::Success);
                }
                break;
            }
            attempts.push(match g.below(10) {
                0..2 => Attempt::Connect,
                2..4 => Attempt::Timeout,
                4 => Attempt::Reset,
                _ => Attempt::Status(g.pick(&statuses)),
            });
        }
        ops.push(RetryOp::Request {
            method: if g.chance(0.01) {
                "get".to_string()
            } else {
                g.pick(&METHODS).to_string()
            },
            key: g.chance(0.1),
            replayable: !g.chance(0.05),
            attempts,
        });
        made += 1;
    }
    ops.push(RetryOp::Balance);
    RetryTrace {
        hosts,
        seed: g.next_u64(),
        retry,
        ops,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ops_round_trip_through_their_lines() {
        for seed in 0..20 {
            let o = Trace::Outlier(generate_outlier(seed, 300));
            assert_eq!(Trace::from_line(&o.to_line()), Ok(o));
            let r = Trace::Retry(generate_retry(seed, 300));
            assert_eq!(Trace::from_line(&r.to_line()), Ok(r));
        }
    }

    #[test]
    fn parsers_follow_the_driver() {
        assert_eq!(
            OutlierOp::parse("f 2 +1 0", 3),
            Ok(OutlierOp::Report {
                ep: 2,
                gateway: true,
                failure: false
            })
        );
        for bad in ["f 3 0 0", "f 0 2 0", "f 0 0", "t -1", "c x", "", "k k", "z"] {
            assert!(OutlierOp::parse(bad, 3).is_err(), "{bad:?}");
        }
        assert_eq!(
            RetryOp::parse("q get 1 0 s 599 c t r"),
            Ok(RetryOp::Request {
                method: "get".into(),
                key: true,
                replayable: false,
                attempts: vec![
                    Attempt::Success,
                    Attempt::Status(599),
                    Attempt::Connect,
                    Attempt::Timeout,
                    Attempt::Reset
                ],
            })
        );
        for bad in [
            "q GET 2 1",
            "q GET 0 1 404",
            "q G(T 0 1",
            "q GET",
            "t x",
            "b b",
            "",
        ] {
            assert!(RetryOp::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn bad_traces_are_refused() {
        let o = generate_outlier(1, 10);
        let mut w = o.clone();
        w.hosts = 0;
        w.ops = vec![OutlierOp::Tick];
        assert!(Trace::from_line(&Trace::Outlier(w).to_line()).is_err());
        let mut w = o.clone();
        w.detector.max_ejection_time = w.detector.base_ejection_time - 1;
        assert!(Trace::from_line(&Trace::Outlier(w).to_line()).is_err());
        let r = generate_retry(1, 10);
        let mut w = r.clone();
        w.retry.window = SECOND - 1;
        assert!(Trace::from_line(&Trace::Retry(w).to_line()).is_err());
        let mut w = r.clone();
        w.retry.retry_on = vec![404];
        assert!(Trace::from_line(&Trace::Retry(w).to_line()).is_err());
        assert!(Trace::from_line(r#"{"op":"picks"}"#).is_err());
        assert!(Trace::from_line("{").is_err());
    }

    #[test]
    fn the_ledger_counts_a_sliding_window() {
        let spec = RetrySpec {
            max_retries: 1,
            retry_on: vec![],
            backoff_base: MILLI,
            backoff_max: MILLI,
            ratio: 0.5,
            min_per_second: 0,
            window: 2 * SECOND,
        };
        let mut l = Ledger::new(&spec);
        assert!(!l.room(0));
        l.at(0).1 += 1;
        assert!(l.room(0));
        l.at(0).2 += 1;
        assert!(!l.room(1));
        l.at(1).1 += 3;
        assert!(l.room(1)); // 1 < 0.5 × 4
        assert!(l.room(2)); // second 0 has left: 0 < 1.5
        l.at(2).2 += 2;
        assert!(!l.room(2));
        assert!(!l.room(10));
    }
}
