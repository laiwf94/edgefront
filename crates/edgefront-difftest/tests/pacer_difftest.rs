//! The pacer differential runner: the Rust side of the protocol against
//! the golden transcripts, generated scripts against the C driver, and the
//! diff against drivers that answer wrongly or die.

use std::fs;
use std::path::{Path, PathBuf};

use edgefront_difftest::PACER_DIFFDRIVE;
use edgefront_difftest::driver;
use edgefront_difftest::pacer::{self, DriverCommand, Plan, Script};

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../legacy/pacer/tests/diffdrive")
}

/// The C driver behind a shell pipeline, such as an awk program that
/// rewrites one of its answers.
fn piped(filter: &str) -> DriverCommand {
    DriverCommand {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            format!("\"$1\" | {filter}"),
            "sh".into(),
            PACER_DIFFDRIVE.into(),
        ],
    }
}

#[test]
fn the_rust_side_prints_the_golden_transcripts() {
    let mut checked = 0;
    for entry in fs::read_dir(golden_dir()).unwrap() {
        let ops = entry.unwrap().path();
        if ops.extension().is_none_or(|e| e != "ops") {
            continue;
        }
        let script = fs::read_to_string(&ops).unwrap();
        let want = fs::read_to_string(ops.with_extension("out")).unwrap();
        let got = pacer::transcript(&script).unwrap();
        let want: Vec<&str> = want.lines().collect();
        for (n, (g, w)) in got.iter().zip(&want).enumerate() {
            assert_eq!(g, w, "{} answer {}", ops.display(), n + 1);
        }
        assert_eq!(got.len(), want.len(), "{}", ops.display());
        checked += 1;
    }
    assert_eq!(checked, 4);
}

#[test]
fn generated_scripts_agree_with_the_c_driver() {
    let plan = Plan {
        scripts: 24,
        ops: 5_000,
        seed: 20_261_104,
    };
    let report = pacer::run_c(plan).unwrap();
    assert_eq!(report.divergence, None, "{report}");
    assert_eq!(report.scripts, 24);
    assert!(report.ops > 24 * 5_000, "{report}");
    // Every decision path on both sides of every check is exercised.
    for outcome in [
        "gcra init rc=0",
        "gcra init rc=EINVAL",
        "gcra take rc=0",
        "gcra take rc=1",
        "gcra take rc=ERANGE",
        "gcra reconfigure rc=EINVAL",
        "gcra take_range",
        "gcra refund",
        "table create rc=0",
        "table create rc=EINVAL",
        "table take rc=1",
        "table take rc=0",
        "table take rc=EFULL",
        "table take rc=EINVAL",
        "table take rc=ERANGE",
        "table take_upto rc=EFULL",
        "table take_range rc=0",
        "table hold rc=1",
        "table hold rc=ELIMIT",
        "table release rc=0",
        "table release rc=ENOENT",
        "table remove rc=0",
        "table remove rc=ENOENT",
        "table get present",
        "table get absent",
        "table get rc=EINVAL",
        "table check check=0",
        "table destroy",
        "table sweep",
        "window init rc=0",
        "window init rc=EINVAL",
        "window add",
        "window count",
        "now",
        "advance",
        "sync",
    ] {
        assert!(
            report.outcomes.get(outcome).is_some_and(|&n| n > 0),
            "{outcome} never happened:\n{report}"
        );
    }
    assert!(!report.outcomes.keys().any(|k| k.contains("EUNKNOWN")));
}

#[test]
fn tables_fill_up_evict_and_reclaim() {
    let script = Script::generate(3, 20_000);
    let answers = pacer::transcript(&script.text()).unwrap();
    let stats: Vec<&String> = answers
        .iter()
        .filter(|a| a.starts_with("entries="))
        .collect();
    for counter in ["evictions", "reclaims", "rejections", "removals", "holds"] {
        let zero = format!(" {counter}=0");
        assert!(
            stats
                .iter()
                .any(|s| s.contains(&format!(" {counter}=")) && !s.contains(&zero)),
            "no table ever counted {counter}"
        );
    }
    // Clamped times, the largest costs and wrapping window counts.
    let text = script.text();
    assert!(text.contains(" 18446744073709551615\n"));
    assert!(
        script
            .lines
            .iter()
            .filter_map(|l| l.strip_prefix("now "))
            .any(|v| v.parse::<u64>().unwrap() > 1 << 57)
    );
}

#[test]
fn a_wrong_answer_is_reported_at_its_step_with_its_seed() {
    let plan = Plan {
        scripts: 3,
        ops: 400,
        seed: 41,
    };
    // Each script runs in a fresh driver, so the first script's 77th
    // answer is the first one forged.
    let driver = piped("awk 'NR == 77 { print \"rc=1 forged\"; next } { print }'");
    let report = pacer::run(&driver, plan).unwrap();
    let d = report
        .divergence
        .as_ref()
        .expect("the forged line is found");
    assert_eq!((d.seed, d.step), (41, 77), "{report}");
    assert_eq!(d.c.as_deref(), Some("rc=1 forged"));
    assert_eq!(d.op, Script::generate(41, 400).lines[76]);
    assert_eq!(d.before.len(), 8);
    assert_eq!(report.scripts, 1);
    assert_eq!(report.ops, 76);
    let text = report.to_string();
    assert!(
        text.contains("divergences: 1\n\nfirst divergence: seed 41 step 77\n"),
        "{text}"
    );
    assert!(text.contains("replay: edgefront-difftest pacer --seed 41 --scripts 1 --ops 400"));
}

#[test]
fn a_divergence_in_a_later_script_names_that_scripts_seed() {
    // Forge an answer only in a run whose script sets the clock first,
    // which odd seeds do and even ones do not.
    let driver = piped(
        "awk 'NR == 1 && $0 ~ /^now=/ { odd = 1 } NR == 30 && odd { print \"x\"; next } { print }'",
    );
    let plan = Plan {
        scripts: 4,
        ops: 300,
        seed: 1_000,
    };
    let report = pacer::run(&driver, plan).unwrap();
    let d = report.divergence.expect("the odd seed's script diverges");
    assert_eq!((d.seed, d.step, d.c.as_deref()), (1_001, 30, Some("x")));
    assert_eq!(report.scripts, 2);
    // Replaying from the reported seed finds the same step at once.
    let replay = pacer::run(
        &driver,
        Plan {
            scripts: 1,
            ops: 300,
            seed: d.seed,
        },
    )
    .unwrap();
    assert_eq!(replay.divergence.map(|r| r.step), Some(30));
}

#[test]
fn a_driver_that_dies_is_reported_with_its_status() {
    let driver = piped("head -n 50; exit 3");
    let report = pacer::run(
        &driver,
        Plan {
            scripts: 1,
            ops: 500,
            seed: 8,
        },
    )
    .unwrap();
    let d = report.divergence.unwrap();
    assert_eq!((d.step, d.c.as_deref()), (51, None));
    assert!(d.rust.is_some());
    assert!(
        d.driver_failure
            .as_deref()
            .unwrap()
            .contains("exit status: 3"),
        "{d}"
    );
}

#[test]
fn a_driver_that_cannot_start_is_an_error() {
    let missing = DriverCommand {
        program: "/nonexistent/pacer-diffdrive".into(),
        args: Vec::new(),
    };
    let plan = Plan {
        scripts: 1,
        ops: 1,
        seed: 0,
    };
    assert!(pacer::run(&missing, plan).is_err());
}

#[test]
fn the_rust_and_c_drivers_reject_the_same_malformed_scripts() {
    for script in [
        "now 1\nbogus\nnow 2\n",
        "gcra 0 take 1\n",
        "table 0 create 1 1 1 1 1 deny 00\n",
        "window 0 init 10\n",
        "now -1\n",
    ] {
        let c = driver::run(Path::new(PACER_DIFFDRIVE), script).unwrap();
        assert_eq!(c.status.code(), Some(2), "{script:?}");
        let rust = pacer::transcript(script).unwrap_err();
        let c_err = c.stderr.trim_end();
        assert!(
            c_err.starts_with(&format!("diffdrive: line {}: ", rust.line)),
            "{script:?}: C said {c_err:?}, Rust {rust}"
        );
        assert_eq!(c.stdout.lines().count(), rust.line - 1, "{script:?}");
    }
}
