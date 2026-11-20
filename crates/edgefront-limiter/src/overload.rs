//! Overload mode: when the host runs short of CPU or memory, limits
//! tighten, so the process keeps serving the clients within them instead
//! of slowing down for everyone.
//!
//! **Pressure** is read by a [`Sampler`]. [`LinuxSampler`] reads the
//! kernel's pressure stall information: `cpu.pressure` and
//! `memory.pressure` in the process's cgroup v2 directory, else
//! `/proc/pressure/cpu` and `/proc/pressure/memory`, taking the `some`
//! line's `avg10` (the share of the last ten seconds in which some task
//! waited for the resource), plus `memory.current` against `memory.max`
//! when the cgroup has a limit. [`BusySampler`] is the in-process
//! alternative, for hosts without PSI (macOS) and for tests: the share
//! of time the workers report being busy.
//!
//! **Levels** ([`Level`]) step up as soon as a reading passes a level's
//! threshold, and down only once readings have stayed below the
//! threshold less a hysteresis margin for `dwell_ns`, one level at a time,
//! so a load hovering at a threshold does not flap.
//!
//! **Scaling.** [`Overload`] keeps the base rate of every table
//! registered with it, and on each change of level reconfigures them to
//! the level's share of it ([`Level::factor`]: all of it, a half, a
//! quarter). A client well within its limit stays within the smaller
//! one; the clients using most of theirs are the ones shed. A reconfigure
//! keeps each key's debt as time (see [`crate::Gcra::reconfigure`]), so
//! no client's bucket is refilled by a level change.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::gcra::Rate;
use crate::table::Table;

/// A pressure reading, each in percent from 0 to 100.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pressure {
    pub cpu: f64,
    pub memory: f64,
}

/// A source of [`Pressure`] readings.
pub trait Sampler: Send {
    fn sample(&mut self) -> Pressure;
}

/// The `some avg10=` value of a PSI file's text.
pub fn psi_avg10(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|f| f.strip_prefix("avg10="))?
        .parse()
        .ok()
}

/// PSI and cgroup memory files.
#[derive(Clone, Debug)]
pub struct LinuxSampler {
    cpu: Option<PathBuf>,
    memory: Option<PathBuf>,
    current: Option<PathBuf>,
    max: Option<PathBuf>,
}

impl LinuxSampler {
    /// Files under the cgroup directory `cgroup` (`/sys/fs/cgroup/...`)
    /// when it has them, else under `proc` (`/proc`).
    pub fn new(cgroup: &Path, proc: &Path) -> Self {
        let pick = |name: &str, fallback: PathBuf| {
            let c = cgroup.join(name);
            if c.exists() {
                Some(c)
            } else if fallback.exists() {
                Some(fallback)
            } else {
                None
            }
        };
        let opt = |name: &str| Some(cgroup.join(name)).filter(|p| p.exists());
        LinuxSampler {
            cpu: pick("cpu.pressure", proc.join("pressure/cpu")),
            memory: pick("memory.pressure", proc.join("pressure/memory")),
            current: opt("memory.current"),
            max: opt("memory.max"),
        }
    }

    /// Whether any pressure file was found.
    pub fn available(&self) -> bool {
        self.cpu.is_some() || self.memory.is_some()
    }
}

fn read(p: &Option<PathBuf>) -> Option<String> {
    std::fs::read_to_string(p.as_ref()?).ok()
}

impl Sampler for LinuxSampler {
    fn sample(&mut self) -> Pressure {
        let cpu = read(&self.cpu).and_then(|t| psi_avg10(&t)).unwrap_or(0.0);
        let mut memory = read(&self.memory)
            .and_then(|t| psi_avg10(&t))
            .unwrap_or(0.0);
        let used = read(&self.current).and_then(|t| t.trim().parse::<f64>().ok());
        let max = read(&self.max).and_then(|t| t.trim().parse::<f64>().ok());
        if let (Some(u), Some(m)) = (used, max)
            && m > 0.0
        {
            memory = memory.max(u * 100.0 / m);
        }
        Pressure {
            cpu: cpu.clamp(0.0, 100.0),
            memory: memory.clamp(0.0, 100.0),
        }
    }
}

/// The share of wall time workers were busy, from what they report.
#[derive(Debug, Default)]
pub struct BusyMeter {
    busy_ns: AtomicU64,
}

impl BusyMeter {
    /// A worker spent `ns` busy.
    pub fn add(&self, ns: u64) {
        self.busy_ns.fetch_add(ns, Ordering::Relaxed);
    }
}

/// Reads a [`BusyMeter`] as CPU pressure over `workers` workers.
pub struct BusySampler {
    meter: Arc<BusyMeter>,
    workers: u64,
    clock: Box<dyn Fn() -> u64 + Send>,
    last: (u64, u64),
}

impl BusySampler {
    pub fn new(meter: Arc<BusyMeter>, workers: u64, clock: Box<dyn Fn() -> u64 + Send>) -> Self {
        let now = clock();
        BusySampler {
            last: (now, meter.busy_ns.load(Ordering::Relaxed)),
            meter,
            workers: workers.max(1),
            clock,
        }
    }
}

impl Sampler for BusySampler {
    fn sample(&mut self) -> Pressure {
        let now = (self.clock)();
        let busy = self.meter.busy_ns.load(Ordering::Relaxed);
        let (t0, b0) = std::mem::replace(&mut self.last, (now, busy));
        let wall = now.saturating_sub(t0).max(1) * self.workers;
        Pressure {
            cpu: (busy.saturating_sub(b0) as f64 * 100.0 / wall as f64).clamp(0.0, 100.0),
            memory: 0.0,
        }
    }
}

/// How hard the host is pressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Normal,
    Elevated,
    Critical,
}

impl Level {
    /// The share of each base rate a level allows.
    pub fn factor(self) -> (u64, u64) {
        match self {
            Level::Normal => (1, 1),
            Level::Elevated => (1, 2),
            Level::Critical => (1, 4),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Level::Normal => "normal",
            Level::Elevated => "elevated",
            Level::Critical => "critical",
        }
    }
}

/// When levels change.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverloadConfig {
    /// CPU pressure entering elevated and critical.
    pub cpu: (f64, f64),
    /// Memory pressure entering elevated and critical.
    pub memory: (f64, f64),
    /// How far below a threshold readings must fall to step down.
    pub hysteresis: f64,
    /// How long they must stay there.
    pub dwell_ns: u64,
}

impl Default for OverloadConfig {
    fn default() -> Self {
        OverloadConfig {
            cpu: (60.0, 85.0),
            memory: (80.0, 92.0),
            hysteresis: 10.0,
            dwell_ns: 5_000_000_000,
        }
    }
}

struct State {
    level: Level,
    /// Since when readings have been low enough to step down.
    calm_since: Option<u64>,
    tables: Vec<(Arc<Table>, Rate)>,
    changes: u64,
}

/// The `[overload]` section of a limiter config: thresholds in percent,
/// the hysteresis margin and the dwell, all optional.
///
/// ```toml
/// [overload]
/// cpu = [60, 85]          # elevated, critical
/// memory = [80, 92]
/// hysteresis = 10
/// dwell = "5s"
/// ```
#[derive(Clone, Debug, Default, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OverloadToml {
    #[serde(default)]
    pub cpu: Option<[f64; 2]>,
    #[serde(default)]
    pub memory: Option<[f64; 2]>,
    #[serde(default)]
    pub hysteresis: Option<f64>,
    #[serde(default)]
    pub dwell: Option<String>,
}

/// A document's `overload` table, ignoring everything else.
#[derive(Clone, Debug, Default, serde::Deserialize, PartialEq)]
pub struct OverloadSection {
    #[serde(default)]
    pub overload: Option<OverloadToml>,
}

impl OverloadToml {
    /// The config, or the mistake as `overload.field: reason`.
    pub fn build(&self) -> Result<OverloadConfig, String> {
        let d = OverloadConfig::default();
        let pair =
            |name: &str, v: Option<[f64; 2]>, dflt: (f64, f64)| -> Result<(f64, f64), String> {
                let (a, b) = v.map_or(dflt, |[a, b]| (a, b));
                if !(0.0 < a && a < b && b <= 100.0) {
                    return Err(format!(
                        "overload.{name}: thresholds must rise within 0 to 100, got [{a}, {b}]"
                    ));
                }
                Ok((a, b))
            };
        let hysteresis = self.hysteresis.unwrap_or(d.hysteresis);
        if !(0.0..50.0).contains(&hysteresis) {
            return Err("overload.hysteresis: must be from 0 to under 50".into());
        }
        let dwell_ns = match &self.dwell {
            None => d.dwell_ns,
            Some(t) => crate::config::parse_duration_text(t)
                .ok_or_else(|| format!("overload.dwell: expected a duration like 5s, got {t:?}"))?,
        };
        Ok(OverloadConfig {
            cpu: pair("cpu", self.cpu, d.cpu)?,
            memory: pair("memory", self.memory, d.memory)?,
            hysteresis,
            dwell_ns,
        })
    }
}

/// The overload controller.
pub struct Overload {
    cfg: OverloadConfig,
    state: Mutex<State>,
}

impl std::fmt::Debug for Overload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Overload")
            .field("level", &self.level())
            .finish()
    }
}

fn scaled(r: &Rate, (num, den): (u64, u64)) -> Rate {
    // A share of the tokens over the same period; the burst too, at
    // least one.
    Rate::new(
        r.tokens.saturating_mul(num),
        r.period_ns.saturating_mul(den),
        (r.burst * num / den).max(1),
    )
}

impl Overload {
    pub fn new(cfg: OverloadConfig) -> Self {
        Overload {
            cfg,
            state: Mutex::new(State {
                level: Level::Normal,
                calm_since: None,
                tables: Vec::new(),
                changes: 0,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Scales `table`, whose rate is `base`, with the level from now on.
    pub fn register(&self, table: Arc<Table>, base: Rate) {
        let mut s = self.lock();
        let _ = table.reconfigure(&scaled(&base, s.level.factor()));
        s.tables.push((table, base));
    }

    pub fn level(&self) -> Level {
        self.lock().level
    }

    /// Level changes so far.
    pub fn changes(&self) -> u64 {
        self.lock().changes
    }

    /// The level a reading calls for on its own.
    fn target(&self, p: Pressure, margin: f64) -> Level {
        let (c, m) = (self.cfg.cpu, self.cfg.memory);
        if p.cpu >= c.1 - margin || p.memory >= m.1 - margin {
            Level::Critical
        } else if p.cpu >= c.0 - margin || p.memory >= m.0 - margin {
            Level::Elevated
        } else {
            Level::Normal
        }
    }

    /// Takes a reading at `now_ns` and returns the level after it.
    pub fn update(&self, p: Pressure, now_ns: u64) -> Level {
        let up = self.target(p, 0.0);
        let mut s = self.lock();
        let next = if up > s.level {
            s.calm_since = None;
            up
        } else if self.target(p, self.cfg.hysteresis) < s.level {
            let since = *s.calm_since.get_or_insert(now_ns);
            if now_ns.saturating_sub(since) >= self.cfg.dwell_ns {
                s.calm_since = Some(now_ns);
                match s.level {
                    Level::Critical => Level::Elevated,
                    _ => Level::Normal,
                }
            } else {
                s.level
            }
        } else {
            s.calm_since = None;
            s.level
        };
        if next != s.level {
            s.level = next;
            s.changes += 1;
            for (t, base) in &s.tables {
                let _ = t.reconfigure(&scaled(base, next.factor()));
            }
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::TableConfig;

    const SEC: u64 = 1_000_000_000;

    fn p(cpu: f64) -> Pressure {
        Pressure { cpu, memory: 0.0 }
    }

    #[test]
    fn the_section_reads_and_checks() {
        let s: OverloadSection =
            toml::from_str("[overload]\ncpu = [50, 90]\ndwell = \"2s\"\n").unwrap();
        let c = s.overload.unwrap().build().unwrap();
        assert_eq!(
            (c.cpu, c.dwell_ns, c.memory),
            ((50.0, 90.0), 2 * SEC, (80.0, 92.0))
        );
        for (t, want) in [
            (
                "[overload]\ncpu = [90, 50]\n",
                "overload.cpu: thresholds must rise",
            ),
            ("[overload]\nhysteresis = 60\n", "overload.hysteresis"),
            ("[overload]\ndwell = \"soon\"\n", "overload.dwell"),
        ] {
            let s: OverloadSection = toml::from_str(t).unwrap();
            assert!(
                s.overload.unwrap().build().unwrap_err().contains(want),
                "{t}"
            );
        }
        let none: OverloadSection = toml::from_str("x = 1\n").unwrap();
        assert!(none.overload.is_none());
    }

    #[test]
    fn psi_files_are_read() {
        let text = "some avg10=12.50 avg60=3.00 avg300=1.00 total=123\nfull avg10=1.00 avg60=0.00 avg300=0.00 total=4\n";
        assert_eq!(psi_avg10(text), Some(12.5));
        assert_eq!(psi_avg10("full avg10=1.00\n"), None);
        assert_eq!(psi_avg10("some avg60=1\n"), None);
        let dir = std::env::temp_dir().join(format!("psi-{}", std::process::id()));
        let cg = dir.join("cg");
        std::fs::create_dir_all(cg.join("x")).unwrap();
        std::fs::create_dir_all(dir.join("proc/pressure")).unwrap();
        std::fs::write(cg.join("cpu.pressure"), text).unwrap();
        std::fs::write(
            dir.join("proc/pressure/memory"),
            "some avg10=5.00 avg60=0 avg300=0 total=0\n",
        )
        .unwrap();
        std::fs::write(cg.join("memory.current"), "900\n").unwrap();
        std::fs::write(cg.join("memory.max"), "1000\n").unwrap();
        let mut s = LinuxSampler::new(&cg, &dir.join("proc"));
        assert!(s.available());
        let r = s.sample();
        assert_eq!(
            r,
            Pressure {
                cpu: 12.5,
                memory: 90.0
            }
        );
        std::fs::write(cg.join("memory.max"), "max\n").unwrap();
        assert_eq!(s.sample().memory, 5.0, "no limit: PSI alone");
        let none = LinuxSampler::new(&dir.join("nope"), &dir.join("nope"));
        assert!(!none.available());
        let mut none = none;
        assert_eq!(none.sample(), Pressure::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn busy_time_reads_as_cpu_pressure() {
        let meter = Arc::new(BusyMeter::default());
        let t = Arc::new(AtomicU64::new(0));
        let tc = Arc::clone(&t);
        let mut s = BusySampler::new(
            Arc::clone(&meter),
            4,
            Box::new(move || tc.load(Ordering::Relaxed)),
        );
        t.store(SEC, Ordering::Relaxed);
        meter.add(3 * SEC);
        assert_eq!(s.sample().cpu, 75.0);
        t.store(2 * SEC, Ordering::Relaxed);
        assert_eq!(s.sample().cpu, 0.0);
    }

    #[test]
    fn levels_step_up_at_once_and_down_after_the_dwell() {
        let o = Overload::new(OverloadConfig::default());
        assert_eq!(o.update(p(30.0), 0), Level::Normal);
        assert_eq!(o.update(p(61.0), SEC), Level::Elevated);
        assert_eq!(o.update(p(90.0), 2 * SEC), Level::Critical);
        // Just under the threshold is not low enough: no step down.
        assert_eq!(o.update(p(80.0), 3 * SEC), Level::Critical);
        assert_eq!(o.update(p(80.0), 30 * SEC), Level::Critical);
        // Under 75 (85 less 10) for the dwell: one step.
        assert_eq!(o.update(p(70.0), 31 * SEC), Level::Critical);
        assert_eq!(o.update(p(70.0), 35 * SEC), Level::Critical);
        assert_eq!(o.update(p(70.0), 36 * SEC), Level::Elevated);
        // A spike resets the calm.
        assert_eq!(o.update(p(20.0), 37 * SEC), Level::Elevated);
        assert_eq!(o.update(p(65.0), 38 * SEC), Level::Elevated);
        assert_eq!(o.update(p(20.0), 39 * SEC), Level::Elevated);
        assert_eq!(o.update(p(20.0), 43 * SEC), Level::Elevated);
        assert_eq!(o.update(p(20.0), 44 * SEC), Level::Normal);
        assert_eq!(o.changes(), 4);
        // Memory alone raises it too.
        assert_eq!(
            o.update(
                Pressure {
                    cpu: 0.0,
                    memory: 95.0
                },
                45 * SEC
            ),
            Level::Critical
        );
        assert!(format!("{o:?}").contains("Critical"));
    }

    /// A load oscillating around a threshold does not flap.
    #[test]
    fn hysteresis_holds_a_hovering_load() {
        let o = Overload::new(OverloadConfig::default());
        let mut changes = 0;
        let mut last = Level::Normal;
        for i in 0..1000u64 {
            let cpu = if i % 2 == 0 { 62.0 } else { 55.0 };
            let l = o.update(p(cpu), i * SEC / 10);
            if l != last {
                changes += 1;
                last = l;
            }
        }
        assert_eq!(changes, 1, "up once, and 55 is not under 50");
    }

    #[test]
    fn registered_tables_scale_with_the_level() {
        let base = Rate::new(100, SEC, 100);
        let t = Arc::new(Table::new(&TableConfig::new(64, base)).unwrap());
        let o = Overload::new(OverloadConfig::default());
        o.register(Arc::clone(&t), base);
        assert_eq!(t.burst(), 100);
        o.update(p(90.0), SEC);
        assert_eq!(t.burst(), 25);
        let now = 10 * SEC;
        let admitted = (0..100)
            .filter(|_| t.take(b"k", now, 1).unwrap().allowed)
            .count();
        assert_eq!(admitted, 25);
        o.update(p(0.0), 2 * SEC);
        o.update(p(0.0), 8 * SEC);
        assert_eq!(o.level(), Level::Elevated);
        assert_eq!(t.burst(), 50);
        assert_eq!(Level::Critical.name(), "critical");
        assert_eq!(scaled(&Rate::new(1, SEC, 1), (1, 4)).burst, 1);
    }
}
