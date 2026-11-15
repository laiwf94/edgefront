//! `edgefront-difftest resilience` end to end: the worked examples that
//! steer's driver answers in `legacy/steer/cmd/diffdrive/resilience_test.go`,
//! a run against the Rust driver, drivers that forge one ejection or one
//! retry decision, and the replay command a divergence prints. The run
//! against steer itself is `make difftest-resilience`, which needs Go.

use std::io::Write;
use std::process::{Command, Output, Stdio};

use edgefront_difftest::resilience::{
    self, Attempt, DetectorSpec, OutlierOp, OutlierTrace, RetryOp, RetrySpec, RetryTrace, Trace,
};

const BIN: &str = env!("CARGO_BIN_EXE_edgefront-difftest");

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("the binary runs")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn outlier_ops(script: &[&str], hosts: usize) -> Vec<OutlierOp> {
    script
        .iter()
        .map(|l| OutlierOp::parse(l, hosts).unwrap())
        .collect()
}

fn retry_ops(script: &[&str]) -> Vec<RetryOp> {
    script.iter().map(|l| RetryOp::parse(l).unwrap()).collect()
}

fn ok(n: usize) -> Vec<&'static str> {
    vec!["ok"; n]
}

/// steer's driver gives these lines for this trace (TestOutlierWorkedExample).
#[test]
fn the_outlier_worked_example_matches_steers_answer() {
    let script = [
        "f 0 1 1",
        "f 0 1 1",
        "f 0 1 1",
        "s",
        "f 1 0 1",
        "f 1 0 1",
        "f 1 0 1",
        "f 1 0 1",
        "f 2 0 0",
        "f 2 0 0",
        "f 2 0 0",
        "f 2 0 0",
        "f 3 0 0",
        "f 3 0 0",
        "f 3 0 0",
        "f 3 0 0",
        "k",
        "f 2 1 1",
        "f 2 1 1",
        "f 2 1 1",
        "t 10000000000",
        "k",
        "f 2 1 1",
        "f 0 1 1",
        "f 0 1 1",
        "f 0 1 1",
        "t 45000000000",
        "k",
        "s",
        "c 2",
        "s",
    ];
    let t = OutlierTrace {
        hosts: 4,
        detector: DetectorSpec {
            consecutive_gateway_failures: 3,
            interval: 1_000_000_000,
            base_ejection_time: 10_000_000_000,
            max_ejection_time: 25_000_000_000,
            max_ejection_percent: 50,
            success_rate_minimum_hosts: 3,
            success_rate_request_volume: 4,
            success_rate_stdev_factor: 1.0,
        },
        ops: outlier_ops(&script, 4),
    };
    let want: Vec<&str> = [
        ok(2),
        vec![
            "eject 0 consecutive_gateway_failure 10000000000",
            "state 1/10000000000 0/- 0/- 0/-",
        ],
        ok(12),
        vec!["eject 1 success_rate 10000000000"],
        ok(3),
        ok(2),
        vec!["eject 2 consecutive_gateway_failure 20000000000"],
        ok(2),
        vec!["eject 0 consecutive_gateway_failure 30000000000"],
        ok(2),
        vec![
            "state 0/30000000000 0/10000000000 0/20000000000 0/-",
            "ok",
            "state 0/30000000000 0/10000000000 0/- 0/-",
        ],
    ]
    .concat();
    let (answer, stats) = resilience::execute_outlier(&t);
    assert!(answer.ok);
    assert_eq!(answer.lines, want);
    assert_eq!(stats.consecutive_ejections, 3);
    assert_eq!(stats.success_rate_ejections, 1);
    assert_eq!(stats.repeat_ejections, 1);
    assert_eq!(stats.ticks, 3);
    assert_eq!(stats.states, 3);
}

/// steer's driver gives these lines for these traces (TestRetryWorkedExample).
#[test]
fn the_retry_worked_example_matches_steers_answer() {
    let spec = RetrySpec {
        max_retries: 2,
        retry_on: vec![502, 503],
        backoff_base: 25_000_000,
        backoff_max: 40_000_000,
        ratio: 0.5,
        min_per_second: 1,
        window: 2_000_000_000,
    };
    let traces = [
        RetryTrace {
            hosts: 3,
            seed: 9,
            retry: spec.clone(),
            ops: retry_ops(&[
                "q GET 0 1 503 c",
                "q get 0 1 503",
                "q POST 0 1 503",
                "q POST 1 1 504",
                "q POST 0 1 c t",
                "q PUT 0 0 c",
                "q DELETE 0 1 r t s",
                "b",
                "t 2000000000",
                "b",
                "q HEAD 0 1 c c c c",
                "b",
            ]),
        },
        RetryTrace {
            hosts: 1,
            seed: 0,
            retry: spec,
            ops: retry_ops(&["q GET 0 1 c", "q OPTIONS 0 1 500 599 s"]),
        },
        RetryTrace {
            hosts: 2,
            seed: 0,
            retry: RetrySpec {
                max_retries: 5,
                retry_on: vec![],
                backoff_base: 1_000_000,
                backoff_max: 1_000_000,
                ratio: 0.0,
                min_per_second: 0,
                window: 1_000_000_000,
            },
            ops: retry_ops(&["q TRACE 0 1 t 503"]),
        },
    ];
    let want: [&[&str]; 3] = [
        &[
            "g r17059068,g r30027795,o d",
            "g d",
            "g d",
            "g d",
            "g r6633061,g d",
            "g d",
            "g r19620342,g r10502136,o d",
            "balance 0.5",
            "ok",
            "balance 2",
            "g r2865202,g r25832349,g d",
            "balance 0.5",
        ],
        &["g n", "f d"],
        &["g x"],
    ];
    for (t, want) in traces.iter().zip(want) {
        let (answer, stats) = resilience::execute_retry(t);
        assert!(answer.ok);
        assert_eq!(answer.lines, want);
        assert_eq!(stats.overdrawn + stats.refused_with_room, 0);
    }
    let (_, stats) = resilience::execute_retry(&traces[0]);
    assert_eq!((stats.requests, stats.retries, stats.balances), (8, 7, 3));
}

#[test]
fn resilience_against_the_rust_driver_finds_nothing() {
    let out = run(&[
        "resilience",
        "--traces",
        "120",
        "--steps",
        "800",
        "--requests",
        "400",
        "--long",
        "20000",
        "--seed",
        "1",
        "--",
        BIN,
        "rust-driver",
    ]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    assert!(
        stdout.starts_with("seed 1\noutlier traces: 120 ("),
        "{stdout}"
    );
    // Seeds 1 to 120 hold one multiple of 100.
    assert!(
        stdout.contains("\nretry traces: 120 (1 of 20000 requests)\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("\nbudget ledger: 0 retries over budget, 0 refused with room\n"),
        "{stdout}"
    );
    assert!(stdout.contains("\ndivergences: 0\n"), "{stdout}");
}

/// Every kind of decision turns up in a modest run, so a run that passes
/// has compared each of them.
#[test]
fn generated_traces_reach_every_decision() {
    let plan = resilience::Plan {
        traces: 200,
        steps: 1_000,
        requests: 500,
        long: 5_000,
        seed: 7,
    };
    struct Ours;
    impl resilience::Oracle for Ours {
        fn ask(&mut self, t: &Trace) -> std::io::Result<edgefront_difftest::balancer::Answer> {
            Ok(resilience::answer(t))
        }
    }
    let r = resilience::run(&mut Ours, plan).unwrap();
    assert_eq!(r.divergence, None);
    assert_eq!(
        (r.outlier_traces, r.retry_traces, r.long_traces),
        (200, 200, 2)
    );
    let o = &r.outlier;
    assert!(o.consecutive_ejections > 500, "{r}");
    assert!(o.success_rate_ejections > 100, "{r}");
    assert!(o.repeat_ejections > 100, "{r}");
    assert!(o.capped_ejections > 10, "{r}");
    let q = &r.retry;
    assert!(q.retries > 10_000, "{r}");
    assert!(q.exhausted > 1_000, "{r}");
    assert!(q.no_endpoint > 1_000, "{r}");
    assert!(q.balances > 500, "{r}");
    assert_eq!(q.overdrawn + q.refused_with_room, 0);
}

/// A driver that answers from the Rust side through `sed`.
fn forging_driver(expr: &str) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        format!("{BIN} rust-driver | sed -u '{expr}'"),
    ]
}

/// The first trace of the kind with a line containing `needle`, from
/// seeds 50 upwards: its index, seed and 1-based step.
fn first_with(kind: &str, needle: &str, plan: resilience::Plan) -> (u64, u64, usize) {
    (0..plan.traces as u64)
        .find_map(|i| {
            let seed = plan.seed + i;
            let t = match kind {
                "outlier" => Trace::Outlier(resilience::generate_outlier(seed, plan.steps)),
                _ => Trace::Retry(resilience::generate_retry(seed, plan.requests_for(seed))),
            };
            let a = resilience::answer(&t);
            a.lines
                .iter()
                .position(|l| l.contains(needle))
                .map(|k| (i, seed, k + 1))
        })
        .expect("a trace with the line")
}

fn plan_args(plan: resilience::Plan) -> Vec<String> {
    [
        "resilience",
        "--traces",
        &plan.traces.to_string(),
        "--steps",
        &plan.steps.to_string(),
        "--requests",
        &plan.requests.to_string(),
        "--long",
        &plan.long.to_string(),
        "--seed",
        &plan.seed.to_string(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn forged_run_reports_and_replays(kind: &str, needle: &str, expr: &str, plan: resilience::Plan) {
    let (index, seed, step) = first_with(kind, needle, plan);
    let driver = forging_driver(expr);
    let mut args = plan_args(plan);
    args.push("--".into());
    args.extend(driver.iter().cloned());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run(&args);
    let stdout = text(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{stdout}\n{}",
        text(&out.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "\n{kind} trace {index} (seed {seed}) step {step}\n"
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains("divergences: 1 (stopped at the first)"),
        "{stdout}"
    );
    assert!(stdout.contains("\nbefore:\n"), "{stdout}");

    let replay = stdout
        .lines()
        .find_map(|l| l.strip_prefix("replay: edgefront-difftest "))
        .expect("a replay line");
    let mut args: Vec<String> = replay
        .strip_suffix(" -- DRIVER")
        .expect("the replay ends with the driver")
        .split(' ')
        .map(String::from)
        .collect();
    args.push("--".into());
    args.extend(driver);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run(&args);
    let again = text(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{again}");
    assert!(
        again.contains(&format!("\n{kind} trace 0 (seed {seed}) step {step}\n")),
        "{again}"
    );
}

#[test]
fn a_forged_ejection_reason_is_reported_and_replays() {
    let plan = resilience::Plan {
        traces: 30,
        steps: 1_500,
        requests: 50,
        long: 50,
        seed: 50,
    };
    forged_run_reports_and_replays(
        "outlier",
        "success_rate",
        "s/ success_rate / consecutive_gateway_failure /",
        plan,
    );
}

#[test]
fn a_forged_budget_refusal_is_reported_and_replays() {
    let plan = resilience::Plan {
        traces: 30,
        steps: 20,
        requests: 2_000,
        long: 2_000,
        seed: 50,
    };
    // The outlier traces are left alone: their lines never hold "g x".
    forged_run_reports_and_replays("retry", "g x", "s/g x/g d/", plan);
}

#[test]
fn resilience_trace_prints_the_query_a_seed_generates() {
    for (kind, args, want) in [
        (
            "outlier",
            vec![
                "resilience-trace",
                "outlier",
                "--steps",
                "300",
                "--seed",
                "9",
            ],
            Trace::Outlier(resilience::generate_outlier(9, 300)),
        ),
        (
            "retry",
            vec![
                "resilience-trace",
                "retry",
                "--requests",
                "300",
                "--seed",
                "9",
            ],
            Trace::Retry(resilience::generate_retry(9, 300)),
        ),
        (
            "long retry",
            vec![
                "resilience-trace",
                "retry",
                "--long",
                "700",
                "--seed",
                "200",
            ],
            Trace::Retry(resilience::generate_retry(200, 700)),
        ),
    ] {
        let out = run(&args);
        assert!(out.status.success(), "{kind}: {}", text(&out.stderr));
        let line = text(&out.stdout);
        assert_eq!(
            Trace::from_line(line.trim_end()),
            Ok(want.clone()),
            "{kind}"
        );
        // Fed by hand to the Rust driver, it is answered.
        let mut child = Command::new(BIN)
            .arg("rust-driver")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();
        let answered = child.wait_with_output().unwrap();
        assert!(answered.status.success(), "{kind}");
        let a: edgefront_difftest::balancer::Answer =
            serde_json::from_slice(&answered.stdout).unwrap();
        assert_eq!(a, resilience::answer(&want), "{kind}");
    }
}

#[test]
fn the_rust_driver_stops_at_a_malformed_resilience_query() {
    let good = Trace::Retry(resilience::generate_retry(1, 10)).to_line();
    let bad = good.replace("\"script\":\"", "\"script\":\"q GET 0 1 404\\n");
    let mut child = Command::new(BIN)
        .arg("rust-driver")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{good}\n{bad}\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(text(&out.stdout).lines().count(), 1);
    assert!(
        text(&out.stderr).contains("line 2: op 1: \"q GET 0 1 404\": malformed"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn bad_resilience_usage_exits_2() {
    for args in [
        vec!["resilience", "--traces", "x", "--", BIN],
        vec!["resilience", "--traces", "1"],
        vec!["resilience", "--bogus", "1", "--", BIN],
        vec!["resilience", "--"],
        vec!["resilience-trace", "picks"],
        vec!["resilience-trace", "outlier", "--traces", "3"],
        vec!["resilience-trace"],
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(text(&out.stderr).contains("usage:"), "{args:?}");
    }
    let out = run(&[
        "resilience",
        "--traces",
        "1",
        "--",
        "/nonexistent/diffdrive",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("(seed "),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn a_trace_that_ends_by_reading_state_is_answered() {
    let t = Trace::Outlier(OutlierTrace {
        hosts: 1,
        detector: DetectorSpec {
            consecutive_gateway_failures: 1,
            interval: 10_000_000,
            base_ejection_time: 1_000_000,
            max_ejection_time: 1_000_000,
            max_ejection_percent: 0,
            success_rate_minimum_hosts: 0,
            success_rate_request_volume: 1,
            success_rate_stdev_factor: 1.9,
        },
        ops: outlier_ops(&["f 0 1 1", "s"], 1),
    });
    // A zero percent turns ejection off.
    assert_eq!(resilience::answer(&t).lines, ["ok", "state 0/-"]);
    let r = Trace::Retry(RetryTrace {
        hosts: 2,
        seed: 1,
        retry: RetrySpec {
            max_retries: 1,
            retry_on: vec![503],
            backoff_base: 1_000_000,
            backoff_max: 1_000_000,
            ratio: 0.0,
            min_per_second: 1,
            window: 1_000_000_000,
        },
        ops: vec![RetryOp::Request {
            method: "PATCH".into(),
            key: true,
            replayable: true,
            attempts: vec![Attempt::Status(503)],
        }],
    });
    let lines = resilience::answer(&r).lines;
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].starts_with("g r") && lines[0].ends_with(",o d"),
        "{lines:?}"
    );
}
