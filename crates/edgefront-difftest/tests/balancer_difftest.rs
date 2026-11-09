//! `edgefront-difftest balancer` end to end: against its own Rust driver,
//! against a driver that forges one pick, and through the replay command
//! it prints. The run against steer itself is `make difftest-balancer`,
//! which needs Go.

use std::io::Write;
use std::process::{Command, Output, Stdio};

use edgefront_difftest::balancer::{self, Scenario};

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

#[test]
fn balancer_against_the_rust_driver_finds_nothing() {
    let out = run(&[
        "balancer",
        "--scenarios",
        "12",
        "--requests",
        "1500",
        "--seed",
        "3",
        "--",
        BIN,
        "rust-driver",
    ]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    assert!(stdout.starts_with("seed 3\nscenarios: 12 ("), "{stdout}");
    assert!(stdout.contains("\ndivergences: 0\n"), "{stdout}");
    assert!(stdout.contains(" picks; panic "), "{stdout}");
}

/// A driver that answers from the Rust side but turns the first request
/// that ran out of endpoints after a pick (`N,-1`) into `N,7`.
fn forging_driver() -> Vec<String> {
    let script = format!(r#"{BIN} rust-driver | sed -u 's/"\([0-9][0-9]*\),-1"/"\1,7"/'"#);
    vec!["sh".into(), "-c".into(), script]
}

/// The step `forging_driver` changes in the scenario of `seed`, if any.
fn forged_step(seed: u64, requests: usize) -> Option<usize> {
    let sc = balancer::generate(seed, requests);
    let a = balancer::answer(&sc);
    a.lines
        .iter()
        .position(|l| {
            l.split_once(',').is_some_and(|(p, rest)| {
                rest == "-1" && !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())
            })
        })
        .map(|k| k + 1)
}

#[test]
fn a_forged_pick_is_reported_at_its_seed_and_step_and_replays() {
    let requests = 600;
    // The first scenario from seed 40 with an exhausted retry.
    let (index, seed, step) = (0..20)
        .find_map(|i| forged_step(40 + i, requests).map(|s| (i, 40 + i, s)))
        .expect("a scenario with an exhausted retry");
    let driver = forging_driver();
    let mut args = vec![
        "balancer".to_string(),
        "--scenarios".into(),
        "20".into(),
        "--requests".into(),
        requests.to_string(),
        "--seed".into(),
        "40".into(),
        "--".into(),
    ];
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
        stdout.contains(&format!("\nscenario {index} (seed {seed}) step {step}\n")),
        "{stdout}"
    );
    assert!(
        stdout.contains("divergences: 1 (stopped at the first)"),
        "{stdout}"
    );
    let sc = balancer::generate(seed, requests);
    assert!(
        stdout.contains(&format!("\nop: {}\n", sc.ops[step - 1])),
        "{stdout}"
    );
    assert!(stdout.contains(",7\nedgefront: "), "{stdout}");

    // The replay command, run as printed with the same driver, finds the
    // same seed and step as scenario 0.
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
        again.contains(&format!("\nscenario 0 (seed {seed}) step {step}\n")),
        "{again}"
    );
}

#[test]
fn balancer_scenario_prints_the_query_a_seed_generates() {
    let out = run(&["balancer-scenario", "--requests", "300", "--seed", "9"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let line = text(&out.stdout);
    assert_eq!(
        Scenario::from_line(line.trim_end()),
        Ok(balancer::generate(9, 300))
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
    assert!(answered.status.success());
    let a: balancer::Answer = serde_json::from_slice(&answered.stdout).unwrap();
    assert_eq!(a, balancer::answer(&balancer::generate(9, 300)));
}

#[test]
fn the_rust_driver_stops_at_a_malformed_picks_query() {
    let mut good = balancer::generate(1, 10).to_line();
    good.push('\n');
    let bad = good.replace("\"script\":\"", "\"script\":\"q\\n");
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
        .write_all(format!("{good}{bad}").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(text(&out.stdout).lines().count(), 1);
    assert!(
        text(&out.stderr).contains("line 2: op 1: \"q\": malformed"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn bad_usage_and_a_missing_driver_exit_2() {
    let out = run(&[
        "balancer",
        "--scenarios",
        "1",
        "--requests",
        "10",
        "--",
        "/nonexistent/diffdrive",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("driver \"/nonexistent/diffdrive\""),
        "{}",
        text(&out.stderr)
    );
    for args in [
        &["balancer"][..],
        &["balancer", "--"],
        &["balancer", "--requests"],
        &["balancer", "--requests", "x", "--", "d"],
        &["balancer", "--bogus", "1", "--", "d"],
        &["balancer-scenario", "--scenarios", "1"],
        &["balancer-scenario", "--", "d"],
    ] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(text(&out.stderr).contains("usage:"), "{args:?}");
    }
    // A driver that dies on the first query is a driver failure, not a
    // divergence.
    let out = run(&[
        "balancer",
        "--scenarios",
        "1",
        "--requests",
        "10",
        "--",
        "true",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("the driver exited without answering"),
        "{}",
        text(&out.stderr)
    );
    // So is one that answers something that is not an answer.
    let out = run(&[
        "balancer",
        "--scenarios",
        "1",
        "--requests",
        "10",
        "--",
        "sh",
        "-c",
        "read l; echo nope",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("bad answer \"nope\""),
        "{}",
        text(&out.stderr)
    );
}
