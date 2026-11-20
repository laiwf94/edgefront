//! Limiter decisions per second: libpacer's `bench/bench_limiter.c` as a
//! scenario. Each workload runs `ops` decisions per thread, every thread
//! starting together, at each thread count, and reports decisions per
//! second across all threads:
//!
//! - `shared-gcra`: one lock-free [`Gcra`] every thread takes from;
//! - `mutex-gcra`: the same algorithm under a mutex, the baseline the C
//!   benchmark kept to show what lock-freedom buys;
//! - `table-spread`: a 200,000-key [`Table`] under the evict policy, each
//!   thread taking from 10,000 keys of its own;
//! - `table-hot`: the same table, every thread taking one key.
//!
//! The rate is 10^7 per second with a burst of 1000 and each thread's
//! clock moves 50 ns per decision, so grants and denials mix, exactly as
//! in the C. [`run_c`] runs the C benchmark with the same thread counts
//! and reads its table, so both sides come out in one [`Report`].

use std::sync::{Arc, Barrier, Mutex};
use std::time::Instant;

use edgefront_limiter::hotkey::{HotConfig, HotTable};
use edgefront_limiter::{FullPolicy, Gcra, Rate, Table, TableConfig};
use serde::{Deserialize, Serialize};

const SEC: u64 = 1_000_000_000;
const RATE: Rate = Rate::new(10_000_000, SEC, 1000);

/// A decisions scenario, as read from TOML.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    /// Thread counts to run each workload at.
    pub threads: Vec<usize>,
    /// Decisions per thread per run.
    pub ops: u64,
    /// Runs per point; the report keeps the median.
    pub runs: usize,
    /// The workloads, by name.
    pub workloads: Vec<Workload>,
}

/// One of bench_limiter.c's four workloads.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum Workload {
    SharedGcra,
    MutexGcra,
    TableSpread,
    TableHot,
    /// One hot key with 10,000 cold ones, all in the table.
    HotKeyTable,
    /// The same traffic through a [`HotTable`], the hot key split.
    HotKeySplit,
}

impl Workload {
    /// The row name bench_limiter.c prints for it.
    pub fn c_row(self) -> &'static str {
        match self {
            Workload::SharedGcra => "gcra, one shared limiter (lock-free)",
            Workload::MutexGcra => "gcra, one shared limiter (mutex baseline)",
            Workload::TableSpread => "table, 10k keys per thread",
            Workload::TableHot => "table, one hot key",
            Workload::HotKeyTable => "hot key, single bucket (no C equivalent)",
            Workload::HotKeySplit => "hot key, sub-buckets (no C equivalent)",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Workload::SharedGcra => "shared-gcra",
            Workload::MutexGcra => "mutex-gcra",
            Workload::TableSpread => "table-spread",
            Workload::TableHot => "table-hot",
            Workload::HotKeyTable => "hot-key-table",
            Workload::HotKeySplit => "hot-key-split",
        }
    }
}

impl Scenario {
    pub fn parse(text: &str) -> Result<Self, String> {
        let s: Scenario = toml::from_str(text).map_err(|e| e.to_string().trim_end().to_string())?;
        if s.threads.is_empty() || s.threads.iter().any(|&t| t == 0 || t > 64) {
            return Err("threads must be 1 to 64 each".into());
        }
        if s.ops == 0 || s.runs == 0 || s.workloads.is_empty() {
            return Err("ops, runs and workloads must not be empty or zero".into());
        }
        Ok(s)
    }
}

/// Decisions per second, in millions, for each workload and thread count,
/// for one implementation.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Series {
    /// `implementation` is `rust` or `c`.
    pub implementation: String,
    /// One row per workload: (workload, millions per second per thread
    /// count, in the scenario's thread order).
    pub rows: Vec<(Workload, Vec<f64>)>,
}

/// Both implementations' series for one scenario.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Report {
    pub scenario: String,
    pub threads: Vec<usize>,
    pub series: Vec<Series>,
}

/// The same GCRA under a mutex: bench_limiter.c's `locked_gcra`.
struct Locked {
    state: Mutex<(u64, u64, u64)>,
}

impl Locked {
    fn new() -> Self {
        let (iv, tol) = RATE.ticks().expect("a valid constant rate");
        Self {
            state: Mutex::new((0, iv, tol)),
        }
    }

    fn take(&self, now: u64) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (tat, iv, tol) = *s;
        let base = tat.max(now);
        let ok = base + iv - now <= tol;
        if ok {
            s.0 = base + iv;
        }
        ok
    }
}

fn table() -> Table {
    let mut c = TableConfig::new(200_000, RATE);
    c.full_policy = FullPolicy::Evict;
    Table::new(&c).expect("a valid constant table")
}

/// One run of `w` with `threads` threads of `ops` decisions each, in
/// decisions per second.
pub fn run_once(w: Workload, threads: usize, ops: u64) -> f64 {
    let g = Arc::new(Gcra::new(&RATE).expect("a valid constant rate"));
    let l = Arc::new(Locked::new());
    let t = Arc::new(table());
    let hot = HotTable::new(
        table(),
        RATE,
        HotConfig {
            parts: threads,
            ..HotConfig::default()
        },
    );
    let go = Arc::new(Barrier::new(threads + 1));
    let hs: Vec<_> = (0..threads)
        .map(|id| {
            let (g, l, t, go) = (g.clone(), l.clone(), t.clone(), go.clone());
            let mut h = hot.handle(id);
            std::thread::spawn(move || {
                go.wait();
                let mut now = SEC;
                let mut allowed = 0u64;
                for i in 0..ops {
                    allowed += u64::from(match w {
                        Workload::SharedGcra => g.take(now, 1).map(|d| d.allowed).unwrap_or(false),
                        // The C baseline works in ticks, `now << 6`.
                        Workload::MutexGcra => l.take(now << 6),
                        Workload::TableSpread => {
                            let k = (i as u32).wrapping_mul(2_654_435_761) % 10_000
                                + id as u32 * 10_000;
                            t.take(&k.to_ne_bytes(), now, 1).is_ok_and(|d| d.allowed)
                        }
                        Workload::TableHot => t.take(b"hot", now, 1).is_ok_and(|d| d.allowed),
                        Workload::HotKeyTable | Workload::HotKeySplit => {
                            // Nine in ten for the hot key, the rest over
                            // 10,000 cold keys.
                            let cold;
                            let key: &[u8] = if i % 10 == 9 {
                                cold =
                                    ((i as u32).wrapping_mul(2_654_435_761) % 10_000).to_ne_bytes();
                                &cold
                            } else {
                                b"hot"
                            };
                            if w == Workload::HotKeyTable {
                                t.take(key, now, 1).is_ok_and(|d| d.allowed)
                            } else {
                                h.take(key, now, 1).is_ok_and(|d| d.allowed)
                            }
                        }
                    });
                    now += 50;
                }
                allowed
            })
        })
        .collect();
    go.wait();
    let start = Instant::now();
    let mut total = 0;
    for h in hs {
        total += h.join().expect("a benchmark thread panicked");
    }
    let secs = start.elapsed().as_secs_f64();
    // Keep the decisions observable so none is optimised away.
    std::hint::black_box(total);
    (ops * threads as u64) as f64 / secs
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// The Rust series: each point the median of `runs` runs, in millions.
pub fn run_rust(s: &Scenario) -> Series {
    let rows = s
        .workloads
        .iter()
        .map(|&w| {
            let row = s
                .threads
                .iter()
                .map(|&t| median((0..s.runs).map(|_| run_once(w, t, s.ops)).collect()) / 1e6)
                .collect();
            (w, row)
        })
        .collect();
    Series {
        implementation: "rust".into(),
        rows,
    }
}

/// Reads bench_limiter.c's output: a header naming the thread counts, then
/// one line per workload, its name and then millions per second.
pub fn parse_c(out: &str, s: &Scenario) -> Result<Series, String> {
    let mut rows = Vec::new();
    for &w in &s.workloads {
        let line = out
            .lines()
            .find(|l| l.starts_with(w.c_row()))
            .ok_or_else(|| format!("no row {:?} in the C output", w.c_row()))?;
        let nums: Vec<f64> = line[w.c_row().len()..]
            .split_whitespace()
            .map(|v| v.parse::<f64>().map_err(|e| format!("{v}: {e}")))
            .collect::<Result<_, _>>()?;
        if nums.len() != s.threads.len() {
            return Err(format!(
                "{} values for {} thread counts in {line:?}",
                nums.len(),
                s.threads.len()
            ));
        }
        rows.push((w, nums));
    }
    Ok(Series {
        implementation: "c".into(),
        rows,
    })
}

/// Runs the C benchmark `bin` with the scenario's thread counts `runs`
/// times and keeps each point's median.
pub fn run_c(bin: &std::path::Path, s: &Scenario) -> Result<Series, String> {
    let threads: Vec<String> = s.threads.iter().map(usize::to_string).collect();
    let mut all: Vec<Series> = Vec::new();
    for _ in 0..s.runs {
        let out = std::process::Command::new(bin)
            .args(&threads)
            .output()
            .map_err(|e| format!("{}: {e}", bin.display()))?;
        if !out.status.success() {
            return Err(format!("{} exited with {}", bin.display(), out.status));
        }
        all.push(parse_c(&String::from_utf8_lossy(&out.stdout), s)?);
    }
    let rows = s
        .workloads
        .iter()
        .enumerate()
        .map(|(i, &w)| {
            let row = (0..s.threads.len())
                .map(|j| median(all.iter().map(|r| r.rows[i].1[j]).collect()))
                .collect();
            (w, row)
        })
        .collect();
    Ok(Series {
        implementation: "c".into(),
        rows,
    })
}

impl Report {
    /// The Markdown table: one row per workload and implementation, one
    /// column per thread count, and Rust against C for each point.
    pub fn markdown(&self) -> String {
        let mut out = String::from("| workload | implementation |");
        for t in &self.threads {
            out.push_str(&format!(" {t} thread{} |", if *t == 1 { "" } else { "s" }));
        }
        out.push_str("\n|---|---|");
        out.push_str(&"---:|".repeat(self.threads.len()));
        out.push('\n');
        let rust = self.series.iter().find(|s| s.implementation == "rust");
        let c = self.series.iter().find(|s| s.implementation == "c");
        if let Some(r) = rust {
            for (i, (w, row)) in r.rows.iter().enumerate() {
                out.push_str(&format!("| {} | Rust |", w.name()));
                for v in row {
                    out.push_str(&format!(" {v:.1} |"));
                }
                out.push('\n');
                if let Some(c) = c.and_then(|c| c.rows.get(i)) {
                    out.push_str(&format!("| {} | C |", w.name()));
                    for v in &c.1 {
                        out.push_str(&format!(" {v:.1} |"));
                    }
                    out.push('\n');
                    out.push_str(&format!("| {} | Rust / C |", w.name()));
                    for (a, b) in row.iter().zip(&c.1) {
                        out.push_str(&format!(" {:.2} |", a / b));
                    }
                    out.push('\n');
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCENARIO: &str = r#"
name = "limiter-decisions"
threads = [1, 2]
ops = 2000
runs = 3
workloads = ["shared-gcra", "mutex-gcra", "table-spread", "table-hot"]
"#;

    #[test]
    fn a_scenario_parses_and_bad_ones_are_refused() {
        let s = Scenario::parse(SCENARIO).unwrap();
        assert_eq!(s.threads, [1, 2]);
        assert_eq!(s.workloads.len(), 4);
        for bad in [
            SCENARIO.replace("[1, 2]", "[]"),
            SCENARIO.replace("[1, 2]", "[0]"),
            SCENARIO.replace("ops = 2000", "ops = 0"),
            SCENARIO.replace("runs = 3", "runs = 0"),
            SCENARIO.replace("\"table-hot\"", "\"nope\""),
            format!("{SCENARIO}\nextra = 1"),
        ] {
            assert!(Scenario::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn every_workload_runs_and_decides() {
        let s = Scenario::parse(SCENARIO).unwrap();
        let r = run_rust(&s);
        assert_eq!(r.rows.len(), 4);
        for (w, row) in &r.rows {
            assert_eq!(row.len(), 2, "{w:?}");
            assert!(row.iter().all(|v| *v > 0.0), "{w:?} {row:?}");
        }
        assert_eq!(median(vec![3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), 2.5);
    }

    #[test]
    fn the_mutex_baseline_admits_as_the_gcra_does() {
        // Single-threaded, both give the same decisions at the same times.
        let g = Gcra::new(&RATE).unwrap();
        let l = Locked::new();
        let mut now = SEC;
        for _ in 0..10_000 {
            assert_eq!(g.take(now, 1).unwrap().allowed, l.take(now << 6));
            now += 50;
        }
    }

    #[test]
    fn the_c_output_is_read_by_row() {
        let s = Scenario::parse(SCENARIO).unwrap();
        let out = "decisions per second (millions)                  1 thread    2 threads\n\
gcra, one shared limiter (lock-free)                364.4        177.6\n\
gcra, one shared limiter (mutex baseline)           199.0         86.7\n\
table, 10k keys per thread                           24.5         30.1\n\
table, one hot key                                   68.4         30.8\n";
        let c = parse_c(out, &s).unwrap();
        assert_eq!(c.rows[0], (Workload::SharedGcra, vec![364.4, 177.6]));
        assert_eq!(c.rows[3], (Workload::TableHot, vec![68.4, 30.8]));
        assert!(parse_c("nothing", &s).is_err());
        let short = out.replace("        177.6", "");
        assert!(parse_c(&short, &s).is_err());
        let rust = Series {
            implementation: "rust".into(),
            rows: c
                .rows
                .iter()
                .map(|(w, r)| (*w, r.iter().map(|v| v * 2.0).collect()))
                .collect(),
        };
        let md = Report {
            scenario: s.name.clone(),
            threads: s.threads.clone(),
            series: vec![rust, c],
        }
        .markdown();
        assert!(
            md.starts_with("| workload | implementation | 1 thread | 2 threads |\n"),
            "{md}"
        );
        assert!(
            md.contains("| shared-gcra | Rust | 728.8 | 355.2 |\n"),
            "{md}"
        );
        assert!(md.contains("| shared-gcra | C | 364.4 | 177.6 |\n"), "{md}");
        assert!(
            md.contains("| shared-gcra | Rust / C | 2.00 | 2.00 |\n"),
            "{md}"
        );
    }
}
