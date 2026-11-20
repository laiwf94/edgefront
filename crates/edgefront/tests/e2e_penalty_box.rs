//! End to end: examples/penalty_box.sh's first two steps against this
//! build's binary: a client boxed for staying over its limit, released on
//! time and banned on its second offence, connections closed after their
//! request quota, and a ban through the `edgefront limits` admin API that
//! survives a reload and expires on time. The third step, overload mode,
//! is edgefront-bench's to run; CI's demo job runs the whole script.

use std::net::TcpListener;
use std::process::Command;

/// The first of five consecutive ports that were all free a moment ago.
fn free_ports() -> u16 {
    for _ in 0..50 {
        let a = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = a.local_addr().unwrap().port();
        if p < u16::MAX - 4 && (1..5).all(|i| TcpListener::bind(("127.0.0.1", p + i)).is_ok()) {
            return p;
        }
    }
    panic!("no five consecutive free ports");
}

#[test]
fn the_penalty_box_demo_passes() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let out = Command::new("bash")
        .arg(format!("{root}/examples/penalty_box.sh"))
        .env("EDGEFRONT_BIN", env!("CARGO_BIN_EXE_edgefront"))
        .env("PENALTY_DEMO_NO_BENCH", "1")
        .env("PENALTY_DEMO_PORT", free_ports().to_string())
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(!stdout.contains("FAIL"), "{stdout}");
    for want in [
        "  ok    boxed: 429\n",
        "  ok    released: 200\n",
        "  ok    second offence: banned: 429\n",
        "  ok    Connection: close on the third response: 1\n",
        "  ok    connections opened: 2\n",
        "  ok    connections served: 3\n",
        "  ok    refused while banned: 0\n",
        "  ok    still refused after the reload: 0\n",
        "  ok    admitted once the ban expired: 1\n",
        "\nall checks passed\n",
    ] {
        assert!(stdout.contains(want), "{want:?} in\n{stdout}");
    }
}
