//! The binary end to end, against its own Rust driver and against drivers
//! that misbehave.

use std::process::{Command, Output};

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
fn steer_against_the_rust_driver_finds_nothing() {
    let out = run(&[
        "steer",
        "--configs",
        "200",
        "--requests",
        "2000",
        "--seed",
        "7",
        "--",
        BIN,
        "rust-driver",
    ]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    assert!(stdout.starts_with("seed 7\nconfigs: 200 ("), "{stdout}");
    assert!(stdout.contains("\nrequests: 2000 ("), "{stdout}");
    assert!(stdout.contains("\ndivergences: 0\n"), "{stdout}");
}

#[test]
fn a_clock_seed_is_printed() {
    let out = run(&[
        "steer",
        "--configs",
        "5",
        "--requests",
        "10",
        "--",
        BIN,
        "rust-driver",
    ]);
    let stdout = text(&out.stdout);
    let seed = stdout
        .lines()
        .next()
        .unwrap()
        .strip_prefix("seed ")
        .unwrap();
    assert!(seed.parse::<u64>().is_ok(), "{stdout}");
}

#[test]
fn a_driver_that_disagrees_fails_the_run() {
    // Rejects everything as a JSON error, which disagrees with every
    // document the Rust side accepts or rejects for another reason.
    let script = r#"while read l; do echo '{"ok":false,"class":"json","error":"nope"}'; done"#;
    let out = run(&[
        "steer",
        "--configs",
        "30",
        "--requests",
        "10",
        "--seed",
        "3",
        "--",
        "sh",
        "-c",
        script,
    ]);
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("\nconfig 0\nconfig: {"), "{stdout}");
}

#[test]
fn too_few_accepted_configs_fail_the_run() {
    // A driver that agrees with nothing but the rejections; with one config
    // there is a fair chance it is rejected by both, so try seeds until one
    // is, and expect the shortfall to be reported.
    for seed in 0..50 {
        let s = seed.to_string();
        let out = run(&[
            "steer",
            "--configs",
            "1",
            "--requests",
            "10",
            "--seed",
            &s,
            "--",
            BIN,
            "rust-driver",
        ]);
        if text(&out.stdout).contains("(0 accepted") {
            assert_eq!(out.status.code(), Some(1));
            assert!(
                text(&out.stderr).contains("only 0 of 10 requests ran"),
                "{}",
                text(&out.stderr)
            );
            return;
        }
    }
    panic!("no seed below 50 generated a rejected document");
}

#[test]
fn driver_failures_and_bad_usage_exit_2() {
    let out = run(&["steer", "--configs", "1", "--", "/nonexistent/diffdrive"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("driver \"/nonexistent/diffdrive\""),
        "{}",
        text(&out.stderr)
    );

    for args in [
        &[][..],
        &["nope"],
        &["steer"],
        &["steer", "--"],
        &["steer", "--configs"],
        &["steer", "--configs", "x", "--", "d"],
        &["steer", "--bogus", "1", "--", "d"],
        &["rust-driver", "extra"],
    ] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(text(&out.stderr).contains("usage:"), "{args:?}");
    }
}

#[test]
fn the_rust_driver_rejects_a_malformed_query() {
    use std::io::Write;
    let mut child = Command::new(BIN)
        .arg("rust-driver")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"op\":\"validate\",\"config\":\"7b7d\"}\n{oops\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stdout).starts_with("{\"ok\":false,\"class\":\"invalid\""),
        "{}",
        text(&out.stdout)
    );
    assert!(
        text(&out.stderr).contains("line 2"),
        "{}",
        text(&out.stderr)
    );
}

fn run_with_stdin(args: &[&str], input: &[u8]) -> Output {
    use std::io::Write;
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn pacer_against_the_c_driver_finds_nothing() {
    let out = run(&["pacer", "--scripts", "5", "--ops", "2000", "--seed", "12"]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    assert!(
        stdout.starts_with("seed 12\nscripts: 5\noperations: "),
        "{stdout}"
    );
    assert!(stdout.contains("table take rc=1 "), "{stdout}");
    assert!(stdout.contains("\ndivergences: 0\n"), "{stdout}");
}

#[test]
fn pacer_against_the_rust_driver_finds_nothing() {
    let out = run(&[
        "pacer",
        "--scripts",
        "2",
        "--ops",
        "1000",
        "--seed",
        "4",
        "--",
        BIN,
        "pacer-rust-driver",
    ]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    assert!(stdout.contains("\ndivergences: 0\n"), "{stdout}");
}

#[test]
fn a_pacer_divergence_prints_the_seed_step_and_replay() {
    let out = run(&[
        "pacer",
        "--scripts",
        "3",
        "--ops",
        "100",
        "--seed",
        "6",
        "--",
        "sh",
        "-c",
        "sed '5s/.*/forged/'",
    ]);
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.contains("first divergence: seed 6 step 1\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("replay: edgefront-difftest pacer --seed 6 --scripts 1 --ops 100\n"),
        "{stdout}"
    );
}

#[test]
fn pacer_script_prints_what_a_seed_generates() {
    let a = run(&["pacer-script", "--seed", "77", "--ops", "300"]);
    let b = run(&["pacer-script", "--ops", "300", "--seed", "77"]);
    assert!(a.status.success());
    assert_eq!(a.stdout, b.stdout);
    let script = text(&a.stdout);
    assert!(script.lines().count() > 300, "{script}");
    // The printed script is what the runner compares: it passes through
    // the Rust driver without a malformed line.
    let out = run_with_stdin(&["pacer-rust-driver"], script.as_bytes());
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout).lines().count(), script.lines().count());
}

#[test]
fn the_pacer_rust_driver_names_a_malformed_line() {
    let out = run_with_stdin(&["pacer-rust-driver"], b"now 3\n# skip\nbogus 1\nnow 4\n");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(text(&out.stdout), "now=3\n");
    assert_eq!(
        text(&out.stderr),
        "diffdrive: line 3: unknown command bogus\n"
    );
}

#[test]
fn pacer_driver_failures_and_bad_usage_exit_2() {
    let out = run(&["pacer", "--ops", "1", "--", "/nonexistent/pacer"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("driver /nonexistent/pacer: "),
        "{}",
        text(&out.stderr)
    );
    for args in [
        &["pacer", "--"][..],
        &["pacer", "--ops"],
        &["pacer", "--ops", "x"],
        &["pacer", "--bogus", "1"],
        &["pacer-script", "--scripts", "1"],
        &["pacer-script", "--", "d"],
        &["pacer-rust-driver", "extra"],
    ] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(text(&out.stderr).contains("usage:"), "{args:?}");
    }
}
