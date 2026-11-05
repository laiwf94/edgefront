//! End to end: the limiter port against libpacer, through the
//! `edgefront-difftest` binary the way a person runs it.
//!
//! - A rate limiting story (an API key limit with an exact retry-after, a
//!   lone limiter and an hourly window quota) gives the same answers from
//!   the C driver and from edgefront-limiter, and the answers are the ones
//!   worked out by hand below.
//! - One million generated operations give identical decisions, retry
//!   times and states from both. Set `EDGEFRONT_E2E_PACER_SEED` to run
//!   them from another seed.
//! - A divergence names its seed and step, and the replay command it
//!   prints reproduces it exactly.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use edgefront_difftest::PACER_DIFFDRIVE;
use edgefront_difftest::driver;
use edgefront_difftest::pacer::Script;

const BIN: &str = env!("CARGO_BIN_EXE_edgefront-difftest");

fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn rust_driver(script: &str) -> Output {
    use std::io::Write;
    let mut child = Command::new(BIN)
        .arg("pacer-rust-driver")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// Each operation of the story, with the answer both sides must give.
const STORY: &[(&str, &str)] = &[
    ("now 1000000000000", "now=1000000000000"),
    // RL1: 100 requests a minute per API key, bursts of 10.
    (
        "table 0 create 1024 0 100 60000000000 10 deny 000102030405060708090a0b0c0d0e0f",
        "rc=0",
    ),
    // "alice" spends her whole burst at once...
    (
        "table 0 take 616c696365 10",
        "rc=1 granted=10 remaining=0 retry=0",
    ),
    // ...so the next request waits exactly one token: 60 s / 100.
    (
        "table 0 take 616c696365 1",
        "rc=0 granted=0 remaining=0 retry=600000000",
    ),
    // "bob" has his own bucket.
    (
        "table 0 take 626f62 1",
        "rc=1 granted=1 remaining=9 retry=0",
    ),
    // One nanosecond early is still denied, with one nanosecond to wait.
    ("advance 599999999", "now=1000599999999"),
    (
        "table 0 take 616c696365 1",
        "rc=0 granted=0 remaining=0 retry=1",
    ),
    // On time is admitted, and the bucket is empty again.
    ("advance 1", "now=1000600000000"),
    (
        "table 0 take 616c696365 1",
        "rc=1 granted=1 remaining=0 retry=0",
    ),
    (
        "table 0 take 616c696365 1",
        "rc=0 granted=0 remaining=0 retry=600000000",
    ),
    // Six seconds refill the whole burst, and no more than it.
    ("advance 6000000000", "now=1006600000000"),
    (
        "table 0 take_upto 616c696365 20",
        "rc=0 granted=10 remaining=0 retry=0",
    ),
    // A lone limiter: one request a second.
    (
        "gcra 0 init 1 1000000000 1",
        "rc=0 tat=0 iv=64000000000 tol=64000000000",
    ),
    (
        "gcra 0 take 1",
        "rc=1 granted=1 remaining=0 retry=0 tat=64486400000000 iv=64000000000 tol=64000000000",
    ),
    (
        "gcra 0 take 1",
        "rc=0 granted=0 remaining=0 retry=1000000000 tat=64486400000000 iv=64000000000 tol=64000000000",
    ),
    // RL2: an hourly quota in four 15-minute buckets.
    (
        "window 0 init 3600000000000 4",
        "rc=0 start=1006600000000 total=0",
    ),
    ("window 0 add 600", "ok start=1006600000000 total=600"),
    ("advance 1800000000000", "now=2806600000000"),
    ("window 0 add 300", "ok start=2806600000000 total=900"),
    ("window 0 count", "count=900 start=2806600000000 total=900"),
    // An hour after the first 600, all of their bucket is still inside.
    ("advance 1800000000000", "now=4606600000000"),
    ("window 0 count", "count=900 start=4606600000000 total=900"),
    // Seven and a half minutes on, half of that bucket has slid out...
    ("advance 450000000000", "now=5056600000000"),
    ("window 0 count", "count=600 start=4606600000000 total=900"),
    // ...and after fifteen it is gone.
    ("advance 450000000000", "now=5506600000000"),
    ("window 0 count", "count=300 start=5506600000000 total=300"),
];

#[test]
fn the_rate_limiting_story_reads_the_same_from_c_and_rust() {
    let script: String = STORY.iter().map(|(op, _)| format!("{op}\n")).collect();
    let c = driver::run(Path::new(PACER_DIFFDRIVE), &script).unwrap();
    assert!(c.success(), "{}: {}", c.status, c.stderr);
    let rust = rust_driver(&script);
    assert!(rust.status.success(), "{}", text(&rust.stderr));
    let rust = text(&rust.stdout);
    let (c, rust): (Vec<&str>, Vec<&str>) = (c.lines().collect(), rust.lines().collect());
    assert_eq!(c.len(), STORY.len());
    assert_eq!(rust.len(), STORY.len());
    for (n, (op, want)) in STORY.iter().enumerate() {
        assert_eq!(c[n], *want, "C, step {}: {op}", n + 1);
        assert_eq!(rust[n], *want, "Rust, step {}: {op}", n + 1);
    }
}

/// The value after `name: ` on the report line that starts with it.
fn field<'a>(report: &'a str, name: &str) -> &'a str {
    report
        .lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(": "))
        .unwrap_or_else(|| panic!("no {name:?} line in\n{report}"))
}

#[test]
fn one_million_operations_agree_with_libpacer() {
    let seed = std::env::var("EDGEFRONT_E2E_PACER_SEED").unwrap_or_else(|_| "20261105".into());
    let out = run(&[
        "pacer",
        "--scripts",
        "100",
        "--ops",
        "10000",
        "--seed",
        &seed,
    ]);
    let report = text(&out.stdout);
    assert!(
        out.status.success(),
        "{report}\n{}\nreplay this run with EDGEFRONT_E2E_PACER_SEED={seed}",
        text(&out.stderr)
    );
    assert!(report.starts_with(&format!("seed {seed}\n")), "{report}");
    assert_eq!(field(&report, "scripts"), "100");
    assert_eq!(field(&report, "divergences"), "0");
    let operations = field(&report, "operations");
    let (count, outcomes) = operations.split_once(' ').unwrap();
    let count: usize = count.parse().unwrap();
    assert!(count >= 1_000_000, "{report}");
    // Decisions both ways, retry times, refunds, evictions and quota
    // counts were all among the compared answers.
    for outcome in [
        "gcra take rc=1 ",
        "gcra take rc=0 ",
        "gcra take_range ",
        "gcra refund ",
        "gcra reconfigure rc=0 ",
        "table take rc=1 ",
        "table take rc=0 ",
        "table take rc=EFULL ",
        "table hold rc=ELIMIT ",
        "table get present ",
        "table stats ",
        "table check check=0 ",
        "window add ",
        "window count ",
    ] {
        assert!(outcomes.contains(outcome), "no {outcome:?} in {report}");
    }
}

/// The C driver, forging the first denied keyed decision (a table take or
/// hold) after line 3,000 whose retry time ends in 7: a bug that depends
/// only on the script, as a real one would.
const FORGING: &str = "\"$1\" | awk 'NR > 3000 && !done && /^rc=0 granted=0 remaining=0 retry=[0-9]*7$/ \
                       { print \"rc=1 granted=1 remaining=0 retry=0\"; done = 1; next } { print }'";

fn forging_driver_args() -> [&'static str; 5] {
    ["sh", "-c", FORGING, "sh", PACER_DIFFDRIVE]
}

#[test]
fn a_divergence_replays_from_the_command_it_prints() {
    let mut args = vec!["pacer", "--scripts", "50", "--ops", "5000", "--seed", "90"];
    args.push("--");
    args.extend(forging_driver_args());
    let out = run(&args);
    let report = text(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{report}{}", text(&out.stderr));

    let first = field(&report, "first divergence");
    let (seed, step) = first
        .strip_prefix("seed ")
        .and_then(|s| s.split_once(" step "))
        .unwrap_or_else(|| panic!("{report}"));
    let (seed, step): (u64, usize) = (seed.parse().unwrap(), step.parse().unwrap());
    assert!((90..140).contains(&seed), "{report}");
    assert!(step > 3000, "{report}");

    // The operation at that step is the one the seed generates, and the
    // C side shows the forged answer.
    let op = &Script::generate(seed, 5000).lines[step - 1];
    let verb = op.split(' ').nth(2).unwrap_or_default();
    assert!(
        op.starts_with("table ") && (verb.starts_with("take") || verb == "hold"),
        "{op}"
    );
    assert!(report.contains(&format!("  {step:>7}  {op}\n")), "{report}");
    assert!(
        report.contains("     C    -> rc=1 granted=1 remaining=0 retry=0\n"),
        "{report}"
    );

    // `pacer-script` prints the same operation at the same line.
    let printed = run(&["pacer-script", "--seed", &seed.to_string(), "--ops", "5000"]);
    assert_eq!(
        text(&printed.stdout).lines().nth(step - 1),
        Some(op.as_str())
    );

    // The printed replay command, run as printed against the same driver,
    // finds the same step of the same seed at once.
    let replay = report
        .lines()
        .find_map(|l| l.strip_prefix("replay: edgefront-difftest "))
        .unwrap_or_else(|| panic!("{report}"));
    let mut replay_args: Vec<&str> = replay.split(' ').collect();
    assert_eq!(
        replay_args,
        [
            "pacer",
            "--seed",
            &seed.to_string(),
            "--scripts",
            "1",
            "--ops",
            "5000"
        ]
    );
    let honest = run(&replay_args);
    replay_args.push("--");
    replay_args.extend(forging_driver_args());
    let again = run(&replay_args);
    let again = text(&again.stdout);
    assert_eq!(field(&again, "scripts"), "1", "{again}");
    assert_eq!(field(&again, "first divergence"), first, "{again}");

    // Against the real C driver the same script agrees.
    assert!(honest.status.success(), "{}", text(&honest.stdout));
    assert_eq!(field(&text(&honest.stdout), "divergences"), "0");
}
