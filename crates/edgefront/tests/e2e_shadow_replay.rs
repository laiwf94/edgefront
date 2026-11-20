//! End to end: examples/shadow_replay.sh against this build's binary, as
//! CI's demo job runs it: a shadow limit that refuses nothing while it
//! records, `edgefront ctl limits replay` against the same and a tighter
//! policy, and a promotion to enforce by editing the file, which enforces
//! at once with the counts the shadow limit already had.

use std::net::TcpListener;
use std::process::Command;

fn free_ports() -> u16 {
    for _ in 0..50 {
        let a = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = a.local_addr().unwrap().port();
        if p < u16::MAX - 2 && (1..3).all(|i| TcpListener::bind(("127.0.0.1", p + i)).is_ok()) {
            return p;
        }
    }
    panic!("no three consecutive free ports");
}

#[test]
fn the_shadow_replay_demo_passes() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let out = Command::new("bash")
        .arg(format!("{root}/examples/shadow_replay.sh"))
        .env("EDGEFRONT_BIN", env!("CARGO_BIN_EXE_edgefront"))
        .env("SHADOW_DEMO_PORT", free_ports().to_string())
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(!stdout.contains("FAIL"), "{stdout}");
    for want in [
        "  ok    k1 answered 200: 20\n",
        "  ok    the same policy: refused before (the would-blocks): 15\n",
        "  ok    the same policy: keys changed: 0\n",
        "  ok    two a minute: refused after: 19\n",
        "  ok    k1, spent while shadowed, is refused at once: 429\n",
        "  ok    k2 has the 2 it had left: 200 200 429 \n",
        "\nall checks passed\n",
    ] {
        assert!(stdout.contains(want), "{want:?} in\n{stdout}");
    }
}
