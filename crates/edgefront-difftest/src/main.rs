//! `edgefront-difftest`: runs the differential suites.
//!
//! ```text
//! edgefront-difftest steer [--configs N] [--requests N] [--seed N] -- DRIVER [ARGS...]
//! edgefront-difftest rust-driver
//! ```
//!
//! `steer` diffs the Rust crates against steer's driver, started as
//! `DRIVER ARGS...` (build it with `go build ./cmd/diffdrive` in
//! `legacy/steer`). It prints a report and exits 1 if anything diverged.
//! Without `--seed` the seed is taken from the clock; it is printed either
//! way, so any run can be replayed.
//!
//! `rust-driver` serves the same protocol from the Rust crates on stdin and
//! stdout, to check the harness itself.

use std::io;
use std::process::ExitCode;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use edgefront_difftest::steer::{self, Driver, Plan};

const USAGE: &str = "usage:
  edgefront-difftest steer [--configs N] [--requests N] [--seed N] -- DRIVER [ARGS...]
  edgefront-difftest rust-driver";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("steer") => match parse_steer(&args[1..]) {
            Ok((plan, driver)) => run_steer(plan, &driver),
            Err(e) => usage(&e),
        },
        Some("rust-driver") if args.len() == 1 => {
            match steer::serve(io::stdin().lock(), io::stdout().lock()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("rust-driver: {e}");
                    ExitCode::from(2)
                }
            }
        }
        _ => usage("expected a command"),
    }
}

fn usage(problem: &str) -> ExitCode {
    eprintln!("edgefront-difftest: {problem}\n{USAGE}");
    ExitCode::from(2)
}

fn parse_steer(args: &[String]) -> Result<(Plan, Vec<String>), String> {
    let mut plan = Plan {
        configs: 10_000,
        requests: 100_000,
        seed: clock_seed(),
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--" {
            let driver: Vec<String> = it.cloned().collect();
            if driver.is_empty() {
                return Err("no driver command after --".into());
            }
            return Ok((plan, driver));
        }
        let value = it.next().ok_or_else(|| format!("{arg} needs a value"))?;
        let number = |v: &str| {
            v.parse::<u64>()
                .map_err(|_| format!("{arg}: {v:?} is not a number"))
        };
        match arg.as_str() {
            "--configs" => plan.configs = number(value)? as usize,
            "--requests" => plan.requests = number(value)? as usize,
            "--seed" => plan.seed = number(value)?,
            _ => return Err(format!("unknown option {arg}")),
        }
    }
    Err("missing -- DRIVER".into())
}

fn clock_seed() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_secs() ^ u64::from(now.subsec_nanos()).rotate_left(32)
}

fn run_steer(plan: Plan, driver: &[String]) -> ExitCode {
    let started = Instant::now();
    let result = Driver::spawn(&driver[0], &driver[1..]).and_then(|mut d| steer::run(&mut d, plan));
    let report = match result {
        Ok(report) => report,
        Err(e) => {
            eprintln!(
                "edgefront-difftest: driver {:?}: {e} (seed {})",
                driver.join(" "),
                plan.seed
            );
            return ExitCode::from(2);
        }
    };
    println!("{report}");
    println!("took {:.1}s", started.elapsed().as_secs_f64());
    if report.divergences > 0 {
        return ExitCode::FAILURE;
    }
    if report.requests < plan.requests {
        eprintln!(
            "edgefront-difftest: only {} of {} requests ran: no generated config was accepted",
            report.requests, plan.requests
        );
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
