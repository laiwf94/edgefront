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
