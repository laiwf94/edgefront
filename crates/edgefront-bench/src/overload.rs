//! Overload mode under CPU saturation, in process: limiter week 8's
//! synthetic test.
//!
//! A pool of `workers` threads serves requests that each burn
//! `service_us` of CPU, so the pool's capacity is `workers / service_us`
//! requests a second. An open-loop generator sends requests on a fixed
//! schedule from `good_clients` clients at `good_rate` each and
//! `abusive_clients` at `abusive_rate`, together about the pool's whole
//! capacity. Every request is first decided by a per-client limit of
//! `client_rate` (a keyed [`Table`]), which both kinds stay within, so the
//! limit alone sheds nothing and the queue grows. With overload mode on,
//! a [`BusySampler`] reads the workers' busy time every `sample_ms` into
//! an [`Overload`] controller, which scales the table's rate down as the
//! pool saturates: the abusive clients, using most of their limit, are
//! shed, and the good ones, well within it, are not.
//!
//! Each request's response time runs from its scheduled send time to the
//! end of its service, so queueing counts. The report gives, after
//! `warmup_s`, the good clients' p50 and p99 and both kinds' goodput, for
//! a run without overload mode and one with it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use edgefront_limiter::overload::{BusyMeter, BusySampler, Overload, OverloadConfig, Sampler};
use edgefront_limiter::{FullPolicy, Rate, Table, TableConfig};
use serde::{Deserialize, Serialize};

/// The scenario, as read from TOML.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    pub workers: usize,
    pub service_us: u64,
    pub duration_s: u64,
    pub warmup_s: u64,
    pub good_clients: u64,
    pub good_rate: f64,
    pub abusive_clients: u64,
    pub abusive_rate: f64,
    pub client_rate: u64,
    pub sample_ms: u64,
    pub dwell_ms: u64,
}

impl Scenario {
    pub fn parse(text: &str) -> Result<Self, String> {
        let s: Scenario = toml::from_str(text).map_err(|e| e.to_string().trim_end().to_owned())?;
        if s.workers == 0
            || s.service_us == 0
            || s.duration_s <= s.warmup_s
            || s.client_rate == 0
            || s.sample_ms == 0
        {
            return Err("workers, service_us, client_rate and sample_ms must be positive, and duration_s longer than warmup_s".into());
        }
        Ok(s)
    }
}

/// One run's numbers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub overload: bool,
    pub good_p50_ms: f64,
    pub good_p99_ms: f64,
    pub good_offered_rps: f64,
    pub good_served_rps: f64,
    pub abusive_offered_rps: f64,
    pub abusive_served_rps: f64,
    pub abusive_shed_rps: f64,
    /// Overload level changes, and the level at the end.
    pub level_changes: u64,
    pub final_level: String,
}

/// Both runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub scenario: String,
    pub runs: Vec<Run>,
}

fn burn(us: u64) {
    let end = Instant::now() + Duration::from_micros(us);
    let mut x = 1u64;
    while Instant::now() < end {
        for _ in 0..64 {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        }
        std::hint::black_box(x);
    }
}

struct Job {
    due: Instant,
    good: bool,
}

/// One run, with overload mode on or off.
pub fn run(s: &Scenario, overload: bool) -> Run {
    let base = Rate::new(s.client_rate, 1_000_000_000, s.client_rate);
    let mut cfg = TableConfig::new(
        ((s.good_clients + s.abusive_clients) as u32 * 2).max(64),
        base,
    );
    cfg.full_policy = FullPolicy::Evict;
    let table = Arc::new(Table::new(&cfg).expect("a valid table"));
    let ctl = Arc::new(Overload::new(OverloadConfig {
        dwell_ns: s.dwell_ms * 1_000_000,
        ..OverloadConfig::default()
    }));
    if overload {
        ctl.register(Arc::clone(&table), base);
    }
    let meter = Arc::new(BusyMeter::default());
    let start = Instant::now();
    let end = start + Duration::from_secs(s.duration_s);
    let measure_from = start + Duration::from_secs(s.warmup_s);
    let (tx, rx) = mpsc::channel::<Job>();
    let rx = Arc::new(Mutex::new(rx));
    let good_times = Arc::new(Mutex::new(Vec::new()));
    let served = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    for _ in 0..s.workers {
        let (rx, meter, good_times, served) = (
            Arc::clone(&rx),
            Arc::clone(&meter),
            Arc::clone(&good_times),
            served.clone(),
        );
        let service = s.service_us;
        threads.push(std::thread::spawn(move || {
            let mut mine = Vec::new();
            loop {
                let job = match rx.lock().unwrap_or_else(|e| e.into_inner()).recv() {
                    Ok(j) => j,
                    Err(_) => break,
                };
                let t0 = Instant::now();
                burn(service);
                let done = Instant::now();
                meter.add((done - t0).as_nanos() as u64);
                if job.due >= measure_from && done <= end + Duration::from_secs(5) {
                    served[usize::from(!job.good)].fetch_add(1, Ordering::Relaxed);
                    if job.good {
                        mine.push((done - job.due).as_nanos() as u64);
                    }
                }
            }
            good_times
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(mine);
        }));
    }
    // The sampler.
    let sampler = {
        let (ctl, meter, stop) = (Arc::clone(&ctl), Arc::clone(&meter), Arc::clone(&stop));
        let workers = s.workers as u64;
        let every = Duration::from_millis(s.sample_ms);
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let clock = move || t0.elapsed().as_nanos() as u64;
            let mut smp = BusySampler::new(meter, workers, Box::new(clock));
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(every);
                let p = smp.sample();
                if overload {
                    ctl.update(p, t0.elapsed().as_nanos() as u64);
                }
            }
        })
    };
    // The open-loop schedule: client i sends every 1/rate seconds, phase
    // shifted by its index.
    let mut offered = [0u64; 2];
    let mut shed = 0u64;
    let total_rate =
        s.good_clients as f64 * s.good_rate + s.abusive_clients as f64 * s.abusive_rate;
    let step = Duration::from_secs_f64(1.0 / total_rate);
    let mut k = 0u64;
    // Each slot goes to a client in proportion to its rate: good ones
    // take the first share of every cycle.
    let good_share = s.good_clients as f64 * s.good_rate / total_rate;
    loop {
        let due = start + step * u32::try_from(k).unwrap_or(u32::MAX);
        if due >= end {
            break;
        }
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        let frac = (k as f64 * 0.618_033_988_75).fract();
        let good = frac < good_share;
        let client = if good {
            k % s.good_clients.max(1)
        } else {
            s.good_clients + k % s.abusive_clients.max(1)
        };
        let measured = due >= measure_from;
        if measured {
            offered[usize::from(!good)] += 1;
        }
        let now_ns = due.duration_since(start).as_nanos() as u64 + 1_000_000_000;
        let admitted = table
            .take(&client.to_be_bytes(), now_ns, 1)
            .is_ok_and(|d| d.allowed);
        if admitted {
            let _ = tx.send(Job { due, good });
        } else if measured && !good {
            shed += 1;
        }
        k += 1;
    }
    drop(tx);
    for t in threads {
        let _ = t.join();
    }
    stop.store(true, Ordering::Relaxed);
    let _ = sampler.join();
    let secs = (s.duration_s - s.warmup_s) as f64;
    let mut times = std::mem::take(&mut *good_times.lock().unwrap_or_else(|e| e.into_inner()));
    times.sort_unstable();
    let pct = |p: f64| {
        if times.is_empty() {
            0.0
        } else {
            times[((times.len() - 1) as f64 * p).round() as usize] as f64 / 1e6
        }
    };
    Run {
        overload,
        good_p50_ms: pct(0.5),
        good_p99_ms: pct(0.99),
        good_offered_rps: offered[0] as f64 / secs,
        good_served_rps: served[0].load(Ordering::Relaxed) as f64 / secs,
        abusive_offered_rps: offered[1] as f64 / secs,
        abusive_served_rps: served[1].load(Ordering::Relaxed) as f64 / secs,
        abusive_shed_rps: shed as f64 / secs,
        level_changes: ctl.changes(),
        final_level: ctl.level().name().to_owned(),
    }
}

impl Report {
    pub fn markdown(&self) -> String {
        let mut out = String::from(
            "| overload mode | good p50 (ms) | good p99 (ms) | good offered / served (req/s) | abusive offered / served / shed (req/s) | level changes | final level |\n|---|---:|---:|---:|---:|---:|---|\n",
        );
        for r in &self.runs {
            out.push_str(&format!(
                "| {} | {:.2} | {:.2} | {:.0} / {:.0} | {:.0} / {:.0} / {:.0} | {} | {} |\n",
                if r.overload { "on" } else { "off" },
                r.good_p50_ms,
                r.good_p99_ms,
                r.good_offered_rps,
                r.good_served_rps,
                r.abusive_offered_rps,
                r.abusive_served_rps,
                r.abusive_shed_rps,
                r.level_changes,
                r.final_level
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scenario_parses_and_a_short_run_reports() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scenarios/overload.toml"
        ))
        .unwrap();
        let s = Scenario::parse(&text).unwrap();
        assert!(s.workers > 0);
        let short = Scenario {
            duration_s: 2,
            warmup_s: 1,
            good_clients: 5,
            abusive_clients: 5,
            good_rate: 20.0,
            abusive_rate: 50.0,
            service_us: 50,
            ..s
        };
        let r = run(&short, true);
        assert!(r.good_offered_rps > 0.0 && r.good_served_rps > 0.0);
        let md = Report {
            scenario: "t".into(),
            runs: vec![r],
        }
        .markdown();
        assert!(md.contains("| on |"), "{md}");
        assert!(Scenario::parse(&text.replace("workers = 4", "workers = 0")).is_err());
    }
}
