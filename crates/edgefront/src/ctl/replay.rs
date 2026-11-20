//! `edgefront ctl limits replay`: a recording of HTTP limit decisions
//! replayed against a candidate policy, with the changes it would make.
//!
//! ```text
//! edgefront ctl limits replay --recording PATH --policy FILE [--top N] [--json FILE]
//! ```
//!
//! `PATH` is a recording file or the directory a `[recording]` section
//! wrote; `FILE` a TOML config whose `[[http_limits]]` are the candidate
//! (see `edgefront_limiter::replay`). The report is printed as tables:
//! each rule's requests, refusals before and after and how many flipped,
//! then the `N` keys whose refusals changed most (20 by default), by the
//! recorder's hash. `--json` also writes the whole report, every changed
//! key included, for review. Exit 0 when the report is written, 2 for bad
//! usage or a file that does not read.

use std::process::ExitCode;

use edgefront_limiter::config::http_limits::{HttpLimitsSection, build};

const USAGE: &str =
    "usage: edgefront ctl limits replay --recording PATH --policy FILE [--top N] [--json FILE]\n";

fn fail(e: String) -> ExitCode {
    eprintln!("edgefront: {e}");
    ExitCode::from(2)
}

/// Runs `replay` with the arguments after it.
pub fn run(args: &[String]) -> ExitCode {
    let (mut recording, mut policy, mut json, mut top) = (None, None, None, 20usize);
    let mut rest = args;
    while let [flag, value, tail @ ..] = rest {
        match flag.as_str() {
            "--recording" => recording = Some(value.clone()),
            "--policy" => policy = Some(value.clone()),
            "--json" => json = Some(value.clone()),
            "--top" => match value.parse() {
                Ok(n) => top = n,
                Err(_) => return fail(format!("--top: {value:?} is not a number")),
            },
            _ => {
                eprint!("{USAGE}");
                return ExitCode::from(2);
            }
        }
        rest = tail;
    }
    let (Some(recording), Some(policy), []) = (recording, policy, rest) else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let rec = match edgefront_limiter::record::read(std::path::Path::new(&recording)) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let rules = std::fs::read_to_string(&policy)
        .map_err(|e| format!("cannot read {policy}: {e}"))
        .and_then(|t| {
            toml::from_str::<HttpLimitsSection>(&t)
                .map_err(|e| format!("{policy}: {}", e.to_string().trim_end()))
        })
        .and_then(|s| build(&s.http_limits).map_err(|e| format!("{policy}: {e}")));
    let rules = match rules {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let started = std::time::Instant::now();
    let report = match edgefront_limiter::replay::replay(&rec, &rules) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    print!("{}", report.text(top));
    println!("\nreplayed in {:.2} s", started.elapsed().as_secs_f64());
    if let Some(j) = json {
        let text = serde_json::to_string_pretty(&report).expect("the report serializes");
        if let Err(e) = std::fs::write(&j, text + "\n") {
            return fail(format!("cannot write {j}: {e}"));
        }
    }
    ExitCode::SUCCESS
}
