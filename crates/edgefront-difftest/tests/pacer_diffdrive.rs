//! The pacer driver built by this crate's build script: the golden
//! transcripts under legacy/pacer/tests/diffdrive, the one-line-per-operation
//! contract the differential runner depends on, and its handling of
//! malformed scripts.

use std::fs;
use std::path::{Path, PathBuf};

use edgefront_difftest::{PACER_DIFFDRIVE, driver};
use edgefront_limiter::Rate;

const HASH_KEY: &str = "0102030405060708090a0b0c0d0e0f10";

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../legacy/pacer/tests/diffdrive")
}

fn run(script: &str) -> driver::Transcript {
    driver::run(Path::new(PACER_DIFFDRIVE), script).expect("run the pacer driver")
}

/// Lines that are operations: not blank and not comments.
fn operations(script: &str) -> usize {
    script
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .count()
}

fn golden_scripts() -> Vec<PathBuf> {
    let mut scripts: Vec<PathBuf> = fs::read_dir(golden_dir())
        .expect("read the golden directory")
        .map(|e| e.expect("directory entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "ops"))
        .collect();
    scripts.sort();
    scripts
}

#[test]
fn golden_transcripts_match() {
    let scripts = golden_scripts();
    let names: Vec<_> = scripts
        .iter()
        .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["errors", "gcra", "table", "window"]);
    for ops in scripts {
        let script = fs::read_to_string(&ops).unwrap();
        let want = fs::read_to_string(ops.with_extension("out")).unwrap();
        let got = run(&script);
        assert!(
            got.success(),
            "{}: {:?} {}",
            ops.display(),
            got.status,
            got.stderr
        );
        assert_eq!(got.stderr, "", "{}", ops.display());
        for (step, (g, w)) in got.lines().zip(want.lines()).enumerate() {
            assert_eq!(
                g,
                w,
                "{} line {} of the transcript",
                ops.display(),
                step + 1
            );
        }
        assert_eq!(
            got.lines().count(),
            want.lines().count(),
            "{}",
            ops.display()
        );
        assert_eq!(
            got.lines().count(),
            operations(&script),
            "{}",
            ops.display()
        );
    }
}

#[test]
fn every_operation_prints_one_line() {
    // One script touching every operation of every kind, with comments,
    // blank lines, tabs and CRLF endings mixed in.
    let script = [
        "# a comment",
        "",
        "now 1000000000",
        "advance 5\r",
        "echo marker 1",
        "sync",
        "gcra 0 init 10 1000000000 5",
        "gcra 0 take 1",
        "gcra 0 take_upto 9",
        "gcra 0 take_range 1 2",
        "gcra 0 refund 1",
        "gcra 0 remaining",
        "gcra 0 reconfigure 20 1000000000 5",
        "gcra 0 state",
        &format!("table 3 create 64 1 10 1000000000 5 evict {HASH_KEY}"),
        "table 3 take 6b 1",
        "table 3 take_upto 6b 9",
        "table 3 take_range 6b 1 2",
        "table 3 refund 6b 1",
        "table 3 hold 6b 4",
        "table 3 release 6b",
        "table 3 remove 6b",
        "table 3 get 6b",
        "table 3 reconfigure 1 1 1",
        "table 3 sweep 1",
        "table 3 stats",
        "table 3 check",
        "table 3 destroy",
        &format!("table 3 create 64 0 10 1000000000 5 deny {HASH_KEY}"),
        "\twindow 15 init 1000 4  ",
        "window 15 add 3",
        "window 15 count",
        "   # an indented comment",
    ]
    .join("\n");
    let got = run(&script);
    assert!(got.success(), "{}", got.stderr);
    assert_eq!(got.lines().count(), operations(&script));
    let lines: Vec<_> = got.lines().collect();
    assert_eq!(lines[0], "now=1000000000");
    assert_eq!(lines[1], "now=1000000005");
    assert_eq!(lines[2], "marker 1");
    assert_eq!(lines[3], "sync");
    assert_eq!(lines[25], "ok");
    assert_eq!(lines[26], "rc=0", "a destroyed table can be created again");
    assert!(lines[29].starts_with("count=3 "), "{}", lines[29]);
}

#[test]
fn rates_convert_to_the_same_ticks_as_the_rust_port() {
    let rates = [
        (10, 1_000_000_000, 5),
        (3, 1_000_000_000, 1),
        (1_000_000_000, 1_000_000_000, 1 << 20),
        (u64::MAX, 1, 1),
        (7, 13, 1_000_000),
        (1, 1 << 40, 1 << 10),
    ];
    let script: String = rates
        .iter()
        .map(|(t, p, b)| format!("gcra 0 init {t} {p} {b}\n"))
        .collect();
    let got = run(&script);
    assert!(got.success(), "{}", got.stderr);
    for (line, &(tokens, period_ns, burst)) in got.lines().zip(&rates) {
        let (iv, tol) = Rate {
            tokens,
            period_ns,
            burst,
        }
        .ticks()
        .unwrap();
        assert_eq!(line, format!("rc=0 tat=0 iv={iv} tol={tol}"));
    }
}

#[test]
fn long_scripts_stream_without_blocking() {
    // Far more output than a pipe buffer holds, so the driver must be
    // answering while the script is still being written.
    let steps = 200_000;
    let mut script = String::from("gcra 0 init 1000 1000000000 100\n");
    for _ in 0..steps {
        script.push_str("advance 1000000\ngcra 0 take 1\n");
    }
    let got = run(&script);
    assert!(got.success(), "{}", got.stderr);
    assert_eq!(got.lines().count(), 1 + 2 * steps);
    assert!(got.lines().last().unwrap().starts_with("rc=1 granted=1 "));
}

#[test]
fn malformed_lines_stop_the_driver() {
    let long_line = format!("echo {}", "x".repeat(5000));
    let cases: &[(&str, &str)] = &[
        ("bogus", "line 1: unknown command bogus"),
        ("now 1\nnow -1", "line 2: not a number: -1"),
        ("now 18446744073709551616", "not a u64"),
        ("now 12x", "not a u64: 12x"),
        ("now", "usage: now N"),
        ("advance 1 2", "usage: advance N"),
        ("sync now", "usage: sync"),
        ("gcra 0 take 1", "gcra 0 is not initialised"),
        ("gcra 16 init 1 1 1", "object index 16 out of range"),
        ("gcra 0", "usage: gcra I OP"),
        (
            "gcra 0 init 1 1 1\ngcra 0 fly",
            "line 2: unknown gcra op fly",
        ),
        ("gcra 0 init 1 1 1\ngcra 0 take", "usage: gcra I take COST"),
        ("table 0 take 61 1", "table 0 does not exist"),
        (
            "table 0 create 1 0 1 1 1 deny 0102",
            "hash key must be 32 hex digits",
        ),
        (
            &format!("table 0 create 1 0 1 1 1 sometimes {HASH_KEY}"),
            "unknown full policy",
        ),
        (
            &format!(
                "table 0 create 1 0 1 1 1 deny {HASH_KEY}\ntable 0 create 1 0 1 1 1 deny {HASH_KEY}"
            ),
            "line 2: table 0 already exists",
        ),
        (
            &format!("table 0 create 4294967296 0 1 1 1 deny {HASH_KEY}"),
            "not a u32",
        ),
        (
            &format!("table 0 create 1 0 1 1 1 deny {HASH_KEY}\ntable 0 get 6"),
            "bad hex of 1 digits",
        ),
        (
            &format!("table 0 create 1 0 1 1 1 deny {HASH_KEY}\ntable 0 get zz"),
            "bad hex: zz",
        ),
        (
            &format!("table 0 create 1 0 1 1 1 deny {HASH_KEY}\ntable 0 get"),
            "usage: table I get KEY",
        ),
        (
            &format!("table 0 create 1 0 1 1 1 deny {HASH_KEY}\ntable 0 jump 61"),
            "unknown table op jump",
        ),
        ("window 0 count", "window 0 is not initialised"),
        (
            "window 0 init 10 1\nwindow 0 slide",
            "unknown window op slide",
        ),
        ("gcra 0 init 1 1 1 1 1 1 1 1 1 1", "more than 12 words"),
        (&long_line, "line 1: line longer than 4094 bytes"),
    ];
    for (script, want) in cases {
        let got = run(script);
        assert_eq!(got.status.code(), Some(2), "{script:?}: {:?}", got.status);
        assert!(
            got.stderr.starts_with("diffdrive: line "),
            "{script:?}: {}",
            got.stderr
        );
        assert!(
            got.stderr.contains(want),
            "{script:?}: want {want:?}, got {}",
            got.stderr
        );
        // Answers to the lines before the bad one are still printed.
        assert_eq!(
            got.lines().count(),
            script.lines().count() - 1,
            "{script:?}"
        );
    }
}

#[test]
fn an_empty_script_prints_nothing() {
    let got = run("");
    assert!(got.success());
    assert_eq!(got.stdout, "");
    let got = run("# only a comment\n\n");
    assert!(got.success());
    assert_eq!(got.stdout, "");
}

#[test]
fn a_driver_that_stops_early_is_not_an_io_error() {
    // The driver exits at line 1 while megabytes of script are still
    // queued; the writer's broken pipe is swallowed and the status reports.
    let mut script = String::from("bogus\n");
    script.push_str(&"advance 1\n".repeat(400_000));
    let got = run(&script);
    assert_eq!(got.status.code(), Some(2));
    assert_eq!(got.stdout, "");
}

#[test]
fn a_missing_driver_is_an_io_error() {
    let err = driver::run(Path::new("/nonexistent/diffdrive"), "").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}
