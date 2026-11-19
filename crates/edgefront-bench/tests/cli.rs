//! The `edgefront-bench` binary's exit codes and messages.

use std::process::Command;

fn bench(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_edgefront-bench"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn help_succeeds() {
    let out = bench(&["--help"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("edgefront-bench run --scenario"));
}

#[test]
fn bad_usage_exits_2_with_the_usage() {
    let out = bench(&["frobnicate"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("unknown command \"frobnicate\"") && err.contains("usage:"),
        "{err}"
    );
}

#[test]
fn a_failure_exits_1_with_its_reason() {
    let out = bench(&["validate", "/no/such/result.json"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.starts_with("edgefront-bench: /no/such/result.json"),
        "{err}"
    );
    assert!(!err.contains("usage:"), "{err}");
}
