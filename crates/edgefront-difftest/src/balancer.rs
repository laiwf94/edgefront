//! Diffs edgefront's balancer against steer's, pick for pick.
//!
//! A [`Scenario`] is one cluster: a policy, a panic threshold, endpoints
//! with weights and priorities, a seed for the balancer's random source,
//! and a script of operations under a manual clock. Requests pick up to a
//! number of attempts, each never repeating an endpoint the request
//! already tried; between them the script flips health verdicts, ejects
//! and readmits endpoints, moves the clock, changes in-flight counts and
//! feeds latency samples, so the scenarios walk through priority spill,
//! panic routing, fallback across levels and running out of endpoints.
//!
//! `legacy/steer/cmd/diffdrive` runs a scenario through steer's
//! `balancer.LB` as a `picks` query (see `balance.go` there for the
//! protocol); [`answer`] runs it through [`edgefront_proxy::Balancer`].
//! Both answer one line per operation, and [`run`] stops at the first line
//! where they differ, reporting the scenario's seed, the step, the
//! operations just before it and the command that replays it.
//!
//! Both balancers draw from SplitMix64 with the scenario's seed, and the
//! clocks start at the same reading, so every random draw and every
//! latency decay matches when the ports agree.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use edgefront_core::clock::{Clock, ManualClock};
use edgefront_core::config::Policy;
use edgefront_proxy::balancer::maglev::hash_key;
use edgefront_proxy::balancer::priority::Decision;
use edgefront_proxy::balancer::rng::SplitMix64;
use edgefront_proxy::balancer::{Balancer, Options};
use edgefront_proxy::endpoint::Endpoint;
use serde::{Deserialize, Serialize};

use crate::rng::Rng;
use crate::steer::Driver;

/// How many operations before a divergence its report shows.
const CONTEXT: usize = 8;

/// One endpoint of a scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointSpec {
    pub address: String,
    pub weight: u32,
    pub priority: u32,
}

/// One operation of a scenario's script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// A request: up to `attempts` picks with this key, each excluding the
    /// endpoints picked before, stopping when there is no pick.
    Request { key: Option<u64>, attempts: u32 },
    /// The priority state.
    State,
    /// Overrides an endpoint's health verdict.
    Health { ep: usize, healthy: bool },
    /// Ejects an endpoint now, for `base` nanoseconds times its ejection
    /// count, capped at `max`.
    Eject { ep: usize, base: u64, max: u64 },
    /// Ends an endpoint's ejection and forgets its multiplier.
    Clear { ep: usize },
    /// One more request in flight on an endpoint.
    Acquire { ep: usize },
    /// One request fewer in flight on an endpoint.
    Release { ep: usize },
    /// An endpoint observes a latency of `nanos` now.
    Observe { ep: usize, nanos: u64 },
    /// The clock moves forward.
    Advance { nanos: u64 },
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Op::Request {
                key: Some(k),
                attempts,
            } => write!(f, "r {k} {attempts}"),
            Op::Request {
                key: None,
                attempts,
            } => write!(f, "r - {attempts}"),
            Op::State => f.write_str("s"),
            Op::Health { ep, healthy } => write!(f, "h {ep} {}", u8::from(healthy)),
            Op::Eject { ep, base, max } => write!(f, "e {ep} {base} {max}"),
            Op::Clear { ep } => write!(f, "c {ep}"),
            Op::Acquire { ep } => write!(f, "a {ep}"),
            Op::Release { ep } => write!(f, "x {ep}"),
            Op::Observe { ep, nanos } => write!(f, "o {ep} {nanos}"),
            Op::Advance { nanos } => write!(f, "t {nanos}"),
        }
    }
}

impl Op {
    /// Parses one script line with the driver's rules: endpoint indices
    /// below `endpoints`, times as non-negative int64 nanoseconds, a
    /// positive ejection base and at least one attempt.
    pub fn parse(line: &str, endpoints: usize) -> Result<Op, String> {
        let f: Vec<&str> = line.split_whitespace().collect();
        // Go's strconv.ParseInt: an optional sign, then decimal digits.
        let int =
            |s: &str| -> Option<u64> { s.parse::<i64>().ok().and_then(|v| u64::try_from(v).ok()) };
        let ep = |s: &str| {
            int(s)
                .and_then(|v| usize::try_from(v).ok())
                .filter(|&v| v < endpoints)
        };
        let op = match f.as_slice() {
            ["r", key, attempts] => {
                let key = match *key {
                    "-" => None,
                    k if !k.starts_with('+') => Some(k.parse::<u64>().ok()),
                    _ => Some(None),
                };
                let attempts = attempts
                    .parse::<i64>()
                    .ok()
                    .and_then(|a| u32::try_from(a).ok());
                match (key, attempts) {
                    (None, Some(attempts)) if attempts >= 1 => Some(Op::Request {
                        key: None,
                        attempts,
                    }),
                    (Some(Some(k)), Some(attempts)) if attempts >= 1 => Some(Op::Request {
                        key: Some(k),
                        attempts,
                    }),
                    _ => None,
                }
            }
            ["s"] => Some(Op::State),
            ["h", e, v] => match (ep(e), *v) {
                (Some(ep), "0") => Some(Op::Health { ep, healthy: false }),
                (Some(ep), "1") => Some(Op::Health { ep, healthy: true }),
                _ => None,
            },
            ["e", e, base, max] => match (ep(e), int(base), int(max)) {
                (Some(ep), Some(base), Some(max)) if base > 0 => Some(Op::Eject { ep, base, max }),
                _ => None,
            },
            ["c", e] => ep(e).map(|ep| Op::Clear { ep }),
            ["a", e] => ep(e).map(|ep| Op::Acquire { ep }),
            ["x", e] => ep(e).map(|ep| Op::Release { ep }),
            ["o", e, nanos] => match (ep(e), int(nanos)) {
                (Some(ep), Some(nanos)) => Some(Op::Observe { ep, nanos }),
                _ => None,
            },
            ["t", nanos] => int(nanos).map(|nanos| Op::Advance { nanos }),
            [] => return Err("empty".into()),
            _ => None,
        };
        op.ok_or_else(|| format!("{line:?}: malformed"))
    }
}

/// One balancing scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    pub policy: Policy,
    pub panic_threshold: u8,
    /// The seed of the balancer's SplitMix64.
    pub seed: u64,
    /// Maglev's table size; zero for the other policies.
    pub table_size: usize,
    pub endpoints: Vec<EndpointSpec>,
    pub ops: Vec<Op>,
}

#[derive(Serialize, Deserialize)]
struct WireScenario {
    op: String,
    policy: String,
    panic_threshold: u8,
    seed: u64,
    table_size: usize,
    endpoints: Vec<EndpointSpec>,
    script: String,
}

/// The name a policy has in configuration.
pub fn policy_name(p: Policy) -> &'static str {
    match p {
        Policy::RoundRobin => "round_robin",
        Policy::LeastLoaded => "least_loaded",
        Policy::Maglev => "maglev",
    }
}

impl Scenario {
    /// The script, one operation per line.
    pub fn script(&self) -> String {
        let mut s = String::with_capacity(self.ops.len() * 10);
        for (i, op) in self.ops.iter().enumerate() {
            if i > 0 {
                s.push('\n');
            }
            s.push_str(&op.to_string());
        }
        s
    }

    /// The scenario as one `picks` query line, without the newline.
    pub fn to_line(&self) -> String {
        let wire = WireScenario {
            op: "picks".into(),
            policy: policy_name(self.policy).into(),
            panic_threshold: self.panic_threshold,
            seed: self.seed,
            table_size: self.table_size,
            endpoints: self.endpoints.clone(),
            script: self.script(),
        };
        serde_json::to_string(&wire).expect("scenarios always serialise")
    }

    /// Parses a line written by [`Scenario::to_line`].
    pub fn from_line(line: &str) -> Result<Scenario, String> {
        let wire: WireScenario = serde_json::from_str(line).map_err(|e| e.to_string())?;
        if wire.op != "picks" {
            return Err(format!("unknown op {:?}", wire.op));
        }
        let policy = match wire.policy.as_str() {
            "round_robin" | "" => Policy::RoundRobin,
            "least_loaded" => Policy::LeastLoaded,
            "maglev" => Policy::Maglev,
            p => return Err(format!("unknown policy {p:?}")),
        };
        let n = wire.endpoints.len();
        let ops = wire
            .script
            .split('\n')
            .enumerate()
            .map(|(i, l)| Op::parse(l, n).map_err(|e| format!("op {}: {e}", i + 1)))
            .collect::<Result<_, _>>()?;
        Ok(Scenario {
            policy,
            panic_threshold: wire.panic_threshold,
            seed: wire.seed,
            table_size: wire.table_size,
            endpoints: wire.endpoints,
            ops,
        })
    }

    /// The number of requests in the script.
    pub fn requests(&self) -> usize {
        self.ops
            .iter()
            .filter(|op| matches!(op, Op::Request { .. }))
            .count()
    }
}

/// An implementation's answer to a scenario: one line per operation, or the
/// reason the balancer could not be built.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub class: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<String>,
}

/// What a scenario exercised, as the Rust balancer saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub requests: usize,
    pub picks: usize,
    /// Requests made while normalized health was below the panic
    /// threshold.
    pub panic: usize,
    /// Requests made while the top level took less than all the traffic.
    pub spill: usize,
    /// Requests whose first attempt went to a level below the top one
    /// while traffic was spread by load.
    pub spilled_picks: usize,
    /// Requests made with nothing available and panic off.
    pub unavailable: usize,
    /// Requests that ran out of endpoints after at least one pick.
    pub exhausted: usize,
    /// Requests with more than one pick.
    pub retried: usize,
}

impl Stats {
    fn add(&mut self, o: &Stats) {
        self.requests += o.requests;
        self.picks += o.picks;
        self.panic += o.panic;
        self.spill += o.spill;
        self.spilled_picks += o.spilled_picks;
        self.unavailable += o.unavailable;
        self.exhausted += o.exhausted;
        self.retried += o.retried;
    }
}

/// Runs a scenario through edgefront's balancer.
pub fn answer(sc: &Scenario) -> Answer {
    execute(sc).0
}

/// [`answer`], with what the scenario exercised.
pub fn execute(sc: &Scenario) -> (Answer, Stats) {
    let clock = Arc::new(ManualClock::new());
    let epoch = clock.start();
    let endpoints: Vec<Endpoint> = sc
        .endpoints
        .iter()
        .map(|e| Endpoint::new(&e.address, e.weight, e.priority, epoch))
        .collect();
    let opts = Options::new(sc.policy)
        .panic_threshold(sc.panic_threshold)
        .clock(Arc::clone(&clock) as Arc<dyn Clock>)
        .rng(Arc::new(SplitMix64::new(sc.seed)))
        .maglev_table_size(sc.table_size);
    let lb = match Balancer::new(endpoints, opts) {
        Ok(lb) => lb,
        Err(e) => {
            let answer = Answer {
                ok: false,
                class: "balancer".into(),
                error: e.to_string(),
                lines: Vec::new(),
            };
            return (answer, Stats::default());
        }
    };
    let eps = lb.endpoints();
    let mut stats = Stats::default();
    let mut lines = Vec::with_capacity(sc.ops.len());
    let mut tried = Vec::new();
    for op in &sc.ops {
        let line = match *op {
            Op::Request { key, attempts } => {
                stats.requests += 1;
                let decision = lb.decision();
                match decision {
                    Decision::Panic => stats.panic += 1,
                    Decision::Unavailable => stats.unavailable += 1,
                    Decision::Spread(loads) if loads.as_slice()[0] < 100 => stats.spill += 1,
                    Decision::Spread(_) => {}
                }
                tried.clear();
                let mut line = String::new();
                for k in 0..attempts {
                    if k > 0 {
                        line.push(',');
                    }
                    match lb.pick(key, &tried) {
                        Some(i) => {
                            line.push_str(&i.to_string());
                            if k == 0
                                && eps[i].priority > 0
                                && matches!(decision, Decision::Spread(_))
                            {
                                stats.spilled_picks += 1;
                            }
                            tried.push(i);
                        }
                        None => {
                            line.push_str("-1");
                            if k > 0 {
                                stats.exhausted += 1;
                            }
                            break;
                        }
                    }
                }
                stats.picks += tried.len();
                if tried.len() > 1 {
                    stats.retried += 1;
                }
                line
            }
            Op::State => {
                let st = lb.state();
                let join = |xs: &[u32]| xs.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
                let loads: Vec<u32> = st.priority_load.iter().map(|&l| u32::from(l)).collect();
                format!(
                    "state h={} t={} l={} panic={}",
                    join(&st.healthy),
                    join(&st.total),
                    join(&loads),
                    st.panic
                )
            }
            Op::Health { ep, healthy } => {
                eps[ep].state.set_healthy(healthy);
                "ok".into()
            }
            Op::Eject { ep, base, max } => {
                eps[ep].state.eject(
                    clock.now(),
                    Duration::from_nanos(base),
                    Duration::from_nanos(max),
                    "difftest",
                );
                "ok".into()
            }
            Op::Clear { ep } => {
                eps[ep].state.clear_ejection();
                "ok".into()
            }
            Op::Acquire { ep } => {
                eps[ep].state.acquire();
                "ok".into()
            }
            Op::Release { ep } => {
                eps[ep].state.release();
                "ok".into()
            }
            Op::Observe { ep, nanos } => {
                eps[ep]
                    .state
                    .observe_latency(Duration::from_nanos(nanos), clock.now());
                "ok".into()
            }
            Op::Advance { nanos } => {
                clock.advance(Duration::from_nanos(nanos));
                "ok".into()
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

/// Answers a `picks` query line from edgefront's crates, for the Rust
/// twin of the driver. A malformed line is an error, as it is for steer's.
pub fn answer_line(line: &str) -> Result<Answer, String> {
    Scenario::from_line(line).map(|sc| answer(&sc))
}

/// Something that runs scenarios: steer's driver, or a stand-in in tests.
pub trait Oracle {
    fn ask(&mut self, sc: &Scenario) -> io::Result<Answer>;
}

impl Oracle for Driver {
    fn ask(&mut self, sc: &Scenario) -> io::Result<Answer> {
        let line = self.ask_line(&sc.to_line())?;
        serde_json::from_str(&line).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad answer {:?}: {e}", short(line.trim_end())),
            )
        })
    }
}

fn short(s: &str) -> String {
    const LIMIT: usize = 400;
    match s.char_indices().nth(LIMIT) {
        Some((i, _)) => format!("{}... ({} bytes)", &s[..i], s.len()),
        None => s.to_string(),
    }
}

/// How much to generate, and from which seed. Scenario `i` is generated
/// from seed `seed + i`, so any one of them can be replayed alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub scenarios: usize,
    pub requests: usize,
    pub seed: u64,
}

/// The first place the implementations disagreed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// Which scenario of the run, from 0.
    pub scenario: usize,
    /// The seed that generates it.
    pub seed: u64,
    /// The 1-based operation, or 0 when the balancers disagreed about the
    /// scenario itself.
    pub step: usize,
    /// The scenario, the operation, both answers and the operations before
    /// it, for a person to read.
    pub detail: String,
}

/// What a run compared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub seed: u64,
    pub requests_per_scenario: usize,
    pub scenarios: usize,
    /// Scenarios by policy.
    pub policies: BTreeMap<&'static str, usize>,
    /// Scenarios both sides refused to build.
    pub rejected: usize,
    pub stats: Stats,
    pub divergence: Option<Divergence>,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = &self.stats;
        writeln!(f, "seed {}", self.seed)?;
        let policies: Vec<String> = self
            .policies
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect();
        writeln!(
            f,
            "scenarios: {} ({}; {} rejected by both)",
            self.scenarios,
            policies.join(", "),
            self.rejected
        )?;
        writeln!(
            f,
            "requests: {} ({} picks; panic {}, spill {}, spilled first picks {}, unavailable {}, retried {}, exhausted {})",
            s.requests,
            s.picks,
            s.panic,
            s.spill,
            s.spilled_picks,
            s.unavailable,
            s.retried,
            s.exhausted
        )?;
        match &self.divergence {
            None => write!(f, "divergences: 0"),
            Some(d) => write!(
                f,
                "divergences: 1 (stopped at the first)\n\nscenario {} (seed {}) step {}\n{}\nreplay: edgefront-difftest balancer --scenarios 1 --requests {} --seed {} -- DRIVER",
                d.scenario, d.seed, d.step, d.detail, self.requests_per_scenario, d.seed
            ),
        }
    }
}

/// Generates `plan.scenarios` scenarios, runs each through `oracle` and
/// edgefront's balancer, and stops at the first line that differs.
pub fn run(oracle: &mut dyn Oracle, plan: Plan) -> io::Result<Report> {
    let mut report = Report {
        seed: plan.seed,
        requests_per_scenario: plan.requests,
        ..Report::default()
    };
    for i in 0..plan.scenarios {
        let seed = plan.seed.wrapping_add(i as u64);
        let sc = generate(seed, plan.requests);
        let theirs = oracle.ask(&sc)?;
        let (ours, stats) = execute(&sc);
        report.scenarios += 1;
        *report.policies.entry(policy_name(sc.policy)).or_default() += 1;
        if let Some(d) = compare(&sc, &theirs, &ours) {
            report.divergence = Some(Divergence {
                scenario: i,
                seed,
                step: d.0,
                detail: d.1,
            });
            return Ok(report);
        }
        if !ours.ok {
            report.rejected += 1;
        }
        report.stats.add(&stats);
    }
    Ok(report)
}

/// The first difference between two answers: its 1-based step (0 for the
/// scenario itself) and a description.
fn compare(sc: &Scenario, theirs: &Answer, ours: &Answer) -> Option<(usize, String)> {
    let head = describe(sc);
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
            sc.ops[j],
            ours.lines[j]
        ));
    }
    let show = |l: Option<&String>| l.map_or("(no line)".to_string(), |l| l.clone());
    let op = sc
        .ops
        .get(k)
        .map_or("(past the end of the script)".to_string(), Op::to_string);
    Some((
        k + 1,
        format!(
            "{head}\nbefore:{before}\nop: {op}\nsteer: {}\nedgefront: {}",
            show(theirs.lines.get(k)),
            show(ours.lines.get(k))
        ),
    ))
}

fn describe(sc: &Scenario) -> String {
    let eps: Vec<String> = sc
        .endpoints
        .iter()
        .map(|e| format!("{} w{} p{}", e.address, e.weight, e.priority))
        .collect();
    format!(
        "policy {} panic_threshold {} rng seed {} table_size {}\nendpoints: {}",
        policy_name(sc.policy),
        sc.panic_threshold,
        sc.seed,
        sc.table_size,
        eps.join(", ")
    )
}

/// Primes for Maglev tables, each larger than the endpoint counts the
/// generator makes below it.
const TABLE_SIZES: [usize; 5] = [101, 211, 1_009, 2_053, 65_537];

/// The scenario `seed` generates, with `requests` requests and the
/// operations between them.
pub fn generate(seed: u64, requests: usize) -> Scenario {
    let mut g = Rng::new(seed);
    let policy = g.pick(&[Policy::RoundRobin, Policy::LeastLoaded, Policy::Maglev]);
    let n = match g.below(100) {
        0..70 => g.range(1, 12),
        70..95 => g.range(13, 40),
        _ => g.range(41, 100),
    };
    let levels = if g.chance(0.1) {
        g.range(1, 8)
    } else {
        g.range(1, n.min(4))
    };
    let mut endpoints: Vec<EndpointSpec> = (0..n)
        .map(|i| EndpointSpec {
            address: match g.below(10) {
                0 => format!("[fd00::{:x}]:8080", i + 1),
                1 => format!("app-{i}.internal:443"),
                _ => format!("10.0.{}.{}:80", i / 250, i % 250 + 1),
            },
            weight: if g.chance(0.1) {
                g.range(1, 1_000) as u32
            } else {
                g.range(1, 10) as u32
            },
            priority: g.below(levels) as u32,
        })
        .collect();
    let panic_threshold = match g.below(8) {
        0 => 0,
        1 => 25,
        2 => 75,
        3 => 100,
        4 => g.range(1, 99) as u8,
        _ => 50,
    };
    let table_size = match policy {
        Policy::Maglev if g.chance(0.03) => 65_537,
        Policy::Maglev => {
            let fits: Vec<usize> = TABLE_SIZES[..4]
                .iter()
                .copied()
                .filter(|&s| s > n)
                .collect();
            g.pick(&fits)
        }
        _ => 0,
    };
    // One scenario in a hundred is one the balancers must refuse.
    let mut table_size = table_size;
    if g.chance(0.01) {
        match g.below(3) {
            0 => endpoints[g.below(n)].weight = 0,
            1 => endpoints[g.below(n)].priority = 8,
            _ if policy == Policy::Maglev => table_size = g.pick(&[n, 100, 1_000]),
            _ => endpoints[g.below(n)].weight = 0,
        }
    }
    let keys: Vec<u64> = (0..64)
        .map(|k| hash_key(format!("session-{seed}-{k}").as_bytes()))
        .collect();
    let mut in_flight = vec![0u32; n];
    let mut ops = Vec::with_capacity(requests * 11 / 10);
    let mut sickness = 0.0;
    let event_rate = if table_size == 65_537 { 0.01 } else { 0.06 };
    for r in 0..requests {
        // The health picture drifts: every 500 requests a new share of the
        // endpoints tends to be unhealthy, from none to all of them.
        if r % 500 == 0 {
            sickness = g.pick(&[0.0, 0.0, 0.1, 0.3, 0.5, 0.7, 0.9, 1.0]);
        }
        if r % 250 == 0 {
            ops.push(Op::State);
        }
        while g.chance(event_rate) {
            let ep = g.below(n);
            let op = match g.below(100) {
                0..40 => Op::Health {
                    ep,
                    healthy: !g.chance(sickness),
                },
                40..52 => {
                    let base = g.range(1, 10_000) as u64 * 1_000_000;
                    Op::Eject {
                        ep,
                        base,
                        max: g.range(0, 60_000) as u64 * 1_000_000,
                    }
                }
                52..57 => Op::Clear { ep },
                57..75 => Op::Advance {
                    nanos: if g.chance(0.1) {
                        g.range(0, 30_000) as u64 * 1_000_000
                    } else {
                        g.below(2_000_000_000) as u64
                    },
                },
                75..83 => {
                    in_flight[ep] += 1;
                    Op::Acquire { ep }
                }
                83..90 if in_flight[ep] > 0 => {
                    in_flight[ep] -= 1;
                    Op::Release { ep }
                }
                83..90 => Op::State,
                _ => Op::Observe {
                    ep,
                    nanos: if g.chance(0.05) {
                        1_000_000_000
                    } else {
                        g.range(1_000, 300_000_000) as u64
                    },
                },
            };
            ops.push(op);
        }
        let key = match policy {
            Policy::Maglev if g.chance(0.8) => Some(if g.chance(0.75) {
                g.pick(&keys)
            } else {
                g.next_u64()
            }),
            Policy::Maglev => None,
            _ if g.chance(0.5) => Some(g.next_u64()),
            _ => None,
        };
        let attempts = match g.below(100) {
            0..75 => 1,
            75..95 => g.range(2, 3) as u32,
            _ => n as u32 + 1,
        };
        ops.push(Op::Request { key, attempts });
    }
    ops.push(Op::State);
    Scenario {
        policy,
        panic_threshold,
        seed: g.next_u64(),
        table_size,
        endpoints,
        ops,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny() -> Scenario {
        Scenario {
            policy: Policy::RoundRobin,
            panic_threshold: 50,
            seed: 7,
            table_size: 0,
            endpoints: vec![
                EndpointSpec {
                    address: "10.0.0.1:80".into(),
                    weight: 1,
                    priority: 0,
                },
                EndpointSpec {
                    address: "10.0.0.2:80".into(),
                    weight: 1,
                    priority: 1,
                },
            ],
            ops: Vec::new(),
        }
    }

    fn with_script(script: &str) -> Scenario {
        let mut sc = tiny();
        sc.ops = script
            .lines()
            .map(|l| Op::parse(l, sc.endpoints.len()).unwrap())
            .collect();
        sc
    }

    // The same scenario as the example in balance.go's test, with steer's
    // answers.
    #[test]
    fn answers_a_worked_example_like_steer() {
        let sc = with_script(
            "r - 3\nr 5 1\nh 0 0\ns\nr - 1\ne 1 1000 5000\nr - 1\nt 2000\nr - 2\na 0\nx 0\no 1 5\nc 1\ns",
        );
        let a = answer(&sc);
        assert!(a.ok);
        assert_eq!(
            a.lines,
            [
                "0,1,-1",
                "0",
                "ok",
                "state h=0,1 t=1,1 l=0,100 panic=false",
                "1",
                "ok",
                "0",
                "ok",
                "1,-1",
                "ok",
                "ok",
                "ok",
                "ok",
                "state h=0,1 t=1,1 l=0,100 panic=false",
            ]
        );
        let (_, stats) = execute(&sc);
        assert_eq!(stats.requests, 5);
        assert_eq!(stats.picks, 6);
        assert_eq!(stats.panic, 1);
        assert_eq!(stats.spill, 2);
        assert_eq!(stats.spilled_picks, 2);
        assert_eq!(stats.exhausted, 2);
        assert_eq!(stats.retried, 1);
    }

    #[test]
    fn ops_round_trip_through_their_lines() {
        let ops = [
            Op::Request {
                key: Some(u64::MAX),
                attempts: 3,
            },
            Op::Request {
                key: None,
                attempts: 1,
            },
            Op::State,
            Op::Health {
                ep: 1,
                healthy: true,
            },
            Op::Health {
                ep: 0,
                healthy: false,
            },
            Op::Eject {
                ep: 1,
                base: 5,
                max: 0,
            },
            Op::Clear { ep: 0 },
            Op::Acquire { ep: 1 },
            Op::Release { ep: 1 },
            Op::Observe { ep: 0, nanos: 9 },
            Op::Advance { nanos: 0 },
        ];
        for op in ops {
            assert_eq!(Op::parse(&op.to_string(), 2), Ok(op), "{op}");
        }
    }

    #[test]
    fn malformed_ops_are_refused_as_steer_refuses_them() {
        for line in [
            "",
            "   ",
            "q",
            "r",
            "r - 0",
            "r - -1",
            "r x 1",
            "r +5 1",
            "r 18446744073709551616 1",
            "s 1",
            "h 2 1",
            "h 0 2",
            "h -1 1",
            "e 0 0 5",
            "e 0 5 -1",
            "e 0 5",
            "c",
            "a 2",
            "x 9",
            "o 0 -3",
            "o 0 9223372036854775808",
            "t -1",
            "t",
        ] {
            assert!(Op::parse(line, 2).is_err(), "{line:?}");
        }
        assert_eq!(Op::parse("", 2), Err("empty".into()));
        assert_eq!(Op::parse("q", 2), Err("\"q\": malformed".into()));
        // Whitespace between fields is free, as with Go's strings.Fields.
        assert_eq!(Op::parse(" t\t5 ", 2), Ok(Op::Advance { nanos: 5 }));
    }

    #[test]
    fn scenarios_round_trip_through_their_query_lines() {
        for seed in 0..30 {
            let sc = generate(seed, 300);
            let line = sc.to_line();
            assert!(line.starts_with(r#"{"op":"picks","policy":""#), "{line}");
            assert_eq!(Scenario::from_line(&line), Ok(sc));
        }
        assert!(Scenario::from_line("{").is_err());
        let bad = tiny().to_line().replace("\"picks\"", "\"route\"");
        assert_eq!(
            Scenario::from_line(&bad),
            Err("unknown op \"route\"".into())
        );
        let bad = with_script("s").to_line().replace("round_robin", "random");
        assert_eq!(
            Scenario::from_line(&bad),
            Err("unknown policy \"random\"".into())
        );
        let bad = with_script("s").to_line().replace("round_robin", "");
        assert_eq!(
            Scenario::from_line(&bad).unwrap().policy,
            Policy::RoundRobin
        );
        let bad = with_script("s\nt 1").to_line().replace("t 1", "t x");
        assert_eq!(
            Scenario::from_line(&bad),
            Err("op 2: \"t x\": malformed".into())
        );
        assert!(answer_line(&bad).is_err());
        assert!(answer_line(&with_script("s").to_line()).unwrap().ok);
    }

    #[test]
    fn a_balancer_that_cannot_be_built_is_an_answer() {
        let mut sc = with_script("s");
        sc.endpoints[1].weight = 0;
        let a = answer(&sc);
        assert!(!a.ok);
        assert_eq!(a.class, "balancer");
        assert_eq!(
            a.error,
            "balancer: endpoint 10.0.0.2:80 has weight 0 priority 1"
        );
        assert!(a.lines.is_empty());
    }

    #[test]
    fn generation_is_deterministic_and_covers_the_ground() {
        assert_eq!(generate(42, 500), generate(42, 500));
        assert_ne!(generate(42, 500), generate(43, 500));
        let mut policies = BTreeMap::new();
        let mut total = Stats::default();
        let mut rejected = 0;
        let mut big_tables = 0;
        for seed in 0..150 {
            let sc = generate(seed, 1_000);
            assert_eq!(sc.requests(), 1_000);
            assert!(matches!(sc.ops.last(), Some(Op::State)));
            *policies.entry(policy_name(sc.policy)).or_insert(0) += 1;
            if sc.policy == Policy::Maglev {
                big_tables += usize::from(sc.table_size == 65_537);
            } else {
                assert_eq!(sc.table_size, 0);
            }
            let (a, stats) = execute(&sc);
            if a.ok {
                assert_eq!(a.lines.len(), sc.ops.len());
            } else {
                rejected += 1;
            }
            total.add(&stats);
        }
        assert_eq!(policies.len(), 3, "{policies:?}");
        assert!(policies.values().all(|&n| n > 35), "{policies:?}");
        assert!((1..10).contains(&rejected), "{rejected}");
        assert!(big_tables > 0);
        for (name, n) in [
            ("panic", total.panic),
            ("spill", total.spill),
            ("spilled", total.spilled_picks),
            ("unavailable", total.unavailable),
            ("retried", total.retried),
            ("exhausted", total.exhausted),
        ] {
            assert!(n > 1_000, "{name}: {n} of {}", total.requests);
        }
    }

    struct Canned(Answer);

    impl Oracle for Canned {
        fn ask(&mut self, _: &Scenario) -> io::Result<Answer> {
            Ok(self.0.clone())
        }
    }

    struct Ours;

    impl Oracle for Ours {
        fn ask(&mut self, sc: &Scenario) -> io::Result<Answer> {
            Ok(answer(sc))
        }
    }

    #[test]
    fn a_run_against_itself_finds_nothing() {
        let plan = Plan {
            scenarios: 20,
            requests: 1_000,
            seed: 11,
        };
        let report = run(&mut Ours, plan).unwrap();
        assert_eq!(report.divergence, None);
        assert_eq!(report.scenarios, 20);
        assert_eq!(report.stats.requests + report.rejected * 1_000, 20_000);
        let text = report.to_string();
        assert!(text.starts_with("seed 11\nscenarios: 20 ("), "{text}");
        assert!(text.ends_with("divergences: 0"), "{text}");
    }

    #[test]
    fn the_first_differing_line_is_reported_with_its_seed_and_step() {
        let sc = generate(5, 100);
        let mut forged = answer(&sc);
        assert!(forged.ok);
        let k = 40;
        forged.lines[k] = "999".into();
        let mut oracle = Canned(forged);
        let report = run(
            &mut oracle,
            Plan {
                scenarios: 3,
                requests: 100,
                seed: 5,
            },
        )
        .unwrap();
        let d = report.divergence.clone().expect("a divergence");
        assert_eq!((d.scenario, d.seed, d.step), (0, 5, k + 1));
        assert!(
            d.detail.contains(&format!("op: {}\n", sc.ops[k])),
            "{}",
            d.detail
        );
        assert!(d.detail.contains("\nsteer: 999\n"), "{}", d.detail);
        assert_eq!(d.detail.matches("\n  ").count(), CONTEXT, "{}", d.detail);
        let text = report.to_string();
        assert!(text.contains("scenario 0 (seed 5) step 41\n"), "{text}");
        assert!(
            text.ends_with("replay: edgefront-difftest balancer --scenarios 1 --requests 100 --seed 5 -- DRIVER"),
            "{text}"
        );
        assert_eq!(report.scenarios, 1);
    }

    #[test]
    fn short_answers_and_disagreements_about_the_scenario_are_divergences() {
        let sc = with_script("r - 1\ns");
        let ours = answer(&sc);
        let mut short = ours.clone();
        short.lines.pop();
        let (step, detail) = compare(&sc, &short, &ours).unwrap();
        assert_eq!(step, 2);
        assert!(detail.contains("\nsteer: (no line)\n"), "{detail}");
        let mut long = ours.clone();
        long.lines.push("ok".into());
        let (step, detail) = compare(&sc, &long, &ours).unwrap();
        assert_eq!(step, 3);
        assert!(
            detail.contains("op: (past the end of the script)"),
            "{detail}"
        );
        let refused = Answer {
            ok: false,
            class: "balancer".into(),
            error: "nope".into(),
            lines: Vec::new(),
        };
        let (step, detail) = compare(&sc, &refused, &ours).unwrap();
        assert_eq!(step, 0);
        assert!(
            detail.contains("steer: ok=false class=\"balancer\" error=\"nope\""),
            "{detail}"
        );
        assert_eq!(compare(&sc, &ours, &ours), None);
        assert_eq!(short.lines.len(), 1);
        assert!(super::short(&"x".repeat(500)).ends_with("... (500 bytes)"));
        assert_eq!(super::short("abc"), "abc");
    }
}
