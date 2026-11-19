//! End to end: examples/l4_limits.sh, pacer's live demo on edgefront,
//! against this build's binary, as CI's demo job runs it. The script
//! starts `edgefront limits` with pacer's demo config and checks the
//! connection-rate limit and its recovery, a slowloris client cut off
//! beside a steady one, and a drain that lets a download in flight finish
//! byte for byte; this test checks that every one of its checks passed.

use std::net::TcpListener;
use std::process::Command;

/// The first of two consecutive ports that were both free a moment ago.
fn two_free_ports() -> u16 {
    for _ in 0..50 {
        let a = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = a.local_addr().unwrap().port();
        if p < u16::MAX && TcpListener::bind(("127.0.0.1", p + 1)).is_ok() {
            return p;
        }
    }
    panic!("no two consecutive free ports");
}

#[test]
fn pacers_live_demo_passes_against_edgefront() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let out = Command::new("bash")
        .arg(format!("{root}/examples/l4_limits.sh"))
        .env("EDGEFRONT_BIN", env!("CARGO_BIN_EXE_edgefront"))
        .env("L4_DEMO_PORT", two_free_ports().to_string())
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert_eq!(stdout.matches("\n  ok    ").count(), 10, "{stdout}");
    assert!(!stdout.contains("FAIL"), "{stdout}");
    for want in [
        "  ok    connections admitted: 5\n",
        "  ok    refused for connection rate: 7\n",
        "  ok    connections admitted after recovery: 5\n",
        "  ok    slowloris cut off: cut\n",
        "  ok    steady client: survived\n",
        "  ok    closed as slow clients: 1\n",
        "  ok    new connections refused while draining: yes\n",
        "  ok    edgefront exit code: 0\n",
        "\ndemo: all checks passed\n",
    ] {
        assert!(stdout.contains(want), "{want:?} in\n{stdout}");
    }
}
