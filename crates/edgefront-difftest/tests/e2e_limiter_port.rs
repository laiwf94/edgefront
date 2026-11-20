//! End to end: the limiter port against libpacer, through the
//! `edgefront-difftest` binary the way a person runs it. libpacer was
//! deleted with legacy/pacer in week 4; its answers were recorded first.
//!
//! - A rate limiting story (an API key limit with an exact retry-after, a
//!   lone limiter and an hourly window quota) gives the answers worked out
//!   by hand below, from the driver binary and from the interpreter.
//! - One million generated operations give exactly libpacer's recorded
//!   transcripts, decisions, retry times and states (`pacer-recorded`),
//!   and so do pacerd's config reader's recorded answers; a changed answer
//!   in the record is caught.
//! - A divergence names its seed and step, and the replay command it
//!   prints reproduces it exactly.

use std::process::{Command, Output, Stdio};

use edgefront_difftest::pacer::{self, Script};

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
fn the_rate_limiting_story_reads_as_worked_out() {
    let script: String = STORY.iter().map(|(op, _)| format!("{op}\n")).collect();
    let inline = pacer::transcript(&script).unwrap();
    let rust = rust_driver(&script);
    assert!(rust.status.success(), "{}", text(&rust.stderr));
    let rust = text(&rust.stdout);
    let rust: Vec<&str> = rust.lines().collect();
    assert_eq!(inline.len(), STORY.len());
    assert_eq!(rust.len(), STORY.len());
    for (n, (op, want)) in STORY.iter().enumerate() {
        assert_eq!(inline[n], *want, "interpreter, step {}: {op}", n + 1);
        assert_eq!(rust[n], *want, "driver binary, step {}: {op}", n + 1);
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
fn one_million_operations_give_libpacers_recorded_answers() {
    let out = run(&["pacer-recorded"]);
    let report = text(&out.stdout);
    assert!(out.status.success(), "{report}\n{}", text(&out.stderr));
    assert_eq!(field(&report, "golden transcripts"), "4 (212 lines)");
    assert_eq!(field(&report, "recorded scripts"), "100 (1013182 lines)");
    assert_eq!(field(&report, "config inputs"), "1739");
    assert_eq!(field(&report, "config mutants"), "2000");
    assert_eq!(field(&report, "differences"), "0");
    // The same scripts through the driver binary: decisions both ways,
    // retry times, refunds, evictions and quota counts are all among the
    // answers the record holds.
    let out = run(&[
        "pacer",
        "--scripts",
        "100",
        "--ops",
        "10000",
        "--seed",
        "20261105",
        "--",
        BIN,
        "pacer-rust-driver",
    ]);
    let report = text(&out.stdout);
    assert!(out.status.success(), "{report}");
    let operations = field(&report, "operations");
    let (count, outcomes) = operations.split_once(' ').unwrap();
    let count: usize = count.parse().unwrap();
    assert_eq!(count, 1_013_182, "{report}");
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

/// Copies the recorded answers to a scratch directory, changes one line
/// with `edit`, and returns the report of checking against the copy.
fn check_changed(file: &str, edit: impl Fn(&str) -> String) -> (Vec<String>, Vec<String>) {
    let src = pacer::recorded_dir();
    let dst = std::env::temp_dir().join(format!("pacer-recorded-{}-{file}", std::process::id()));
    let copy = |rel: &str| {
        let to = dst.join(rel);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(src.join(rel), to).unwrap();
    };
    for rel in [
        "recorded/algorithms.sha256",
        "recorded/config-answers.jsonl",
        "recorded/config-literals.jsonl",
        "recorded/config-mutants.sha256",
        "fuzz/config.dict",
    ] {
        copy(rel);
    }
    for dir in ["tests", "examples", "fuzz/corpus"] {
        let mut stack = vec![src.join(dir)];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    copy(&p.strip_prefix(&src).unwrap().to_string_lossy());
                }
            }
        }
    }
    let path = dst.join("recorded").join(file);
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, edit(&text)).unwrap();
    let algo = pacer::check_recorded(&dst, Some(3)).unwrap().mismatches;
    let cfg = edgefront_difftest::pacer_config::check_recorded(&dst)
        .unwrap()
        .mismatches;
    std::fs::remove_dir_all(&dst).unwrap();
    (algo, cfg)
}

#[test]
fn a_changed_recorded_answer_is_caught() {
    // A script's digest off by one digit.
    let (algo, cfg) = check_changed("algorithms.sha256", |t| {
        let (first, rest) = t.split_once('\n').unwrap();
        let flipped = if first.ends_with('0') { "1" } else { "0" };
        format!("{}{flipped}\n{rest}", &first[..first.len() - 1])
    });
    assert_eq!(algo.len(), 1, "{algo:?}");
    assert!(algo[0].starts_with("seed 20261105: "), "{algo:?}");
    assert!(cfg.is_empty());
    // A config answer changed.
    let (algo, cfg) = check_changed("config-answers.jsonl", |t| {
        t.replacen("workers=2 ", "workers=3 ", 1)
    });
    assert!(algo.is_empty());
    assert_eq!(cfg.len(), 1, "{cfg:?}");
    assert!(cfg[0].contains("pacerd:    ok | workers=3 "), "{cfg:?}");
    // A mutant's digest changed.
    let (_, cfg) = check_changed("config-mutants.sha256", |t| t.replacen(" ", " f", 1));
    assert_eq!(cfg.len(), 1, "{cfg:?}");
    assert!(cfg[0].starts_with("mutant 1: "), "{cfg:?}");
}

/// The driver, forging the first denied keyed decision (a table take or
/// hold) after line 3,000 whose retry time ends in 7: a bug that depends
/// only on the script, as a real one would.
const FORGING: &str = "\"$1\" pacer-rust-driver | awk 'NR > 3000 && !done && /^rc=0 granted=0 remaining=0 retry=[0-9]*7$/ \
                       { print \"rc=1 granted=1 remaining=0 retry=0\"; done = 1; next } { print }'";

fn forging_driver_args() -> [&'static str; 5] {
    ["sh", "-c", FORGING, "sh", BIN]
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
    // driver's side shows the forged answer.
    let op = &Script::generate(seed, 5000).lines[step - 1];
    let verb = op.split(' ').nth(2).unwrap_or_default();
    assert!(
        op.starts_with("table ") && (verb.starts_with("take") || verb == "hold"),
        "{op}"
    );
    assert!(report.contains(&format!("  {step:>7}  {op}\n")), "{report}");
    assert!(
        report.contains("   driver -> rc=1 granted=1 remaining=0 retry=0\n"),
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
        .and_then(|l| l.strip_suffix(" -- DRIVER [ARGS...]"))
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
    let mut honest_args = replay_args.clone();
    honest_args.extend(["--", BIN, "pacer-rust-driver"]);
    let honest = run(&honest_args);
    replay_args.push("--");
    replay_args.extend(forging_driver_args());
    let again = run(&replay_args);
    let again = text(&again.stdout);
    assert_eq!(field(&again, "scripts"), "1", "{again}");
    assert_eq!(field(&again, "first divergence"), first, "{again}");

    // Against the honest driver the same script agrees.
    assert!(honest.status.success(), "{}", text(&honest.stdout));
    assert_eq!(field(&text(&honest.stdout), "divergences"), "0");
}
