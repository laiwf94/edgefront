//! End to end: examples/http_limits.sh against this build's binary, as
//! CI's demo job runs it. The script starts `edgefront proxy --limits` in
//! front of a small upstream and checks a login path limited on failed
//! attempts only, an API limited per API key header, the RateLimit and
//! Retry-After headers, and `edgefront check` on the rules; this test
//! checks that every one of its checks passed.

use std::net::TcpListener;
use std::process::Command;

/// The first of three consecutive ports that were all free a moment ago.
fn three_free_ports() -> u16 {
    for _ in 0..50 {
        let a = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = a.local_addr().unwrap().port();
        if p < u16::MAX - 2
            && TcpListener::bind(("127.0.0.1", p + 1)).is_ok()
            && TcpListener::bind(("127.0.0.1", p + 2)).is_ok()
        {
            return p;
        }
    }
    panic!("no three consecutive free ports");
}

#[test]
fn the_http_limits_demo_passes() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let out = Command::new("bash")
        .arg(format!("{root}/examples/http_limits.sh"))
        .env("EDGEFRONT_BIN", env!("CARGO_BIN_EXE_edgefront"))
        .env("HTTP_DEMO_PORT", three_free_ports().to_string())
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(!stdout.contains("FAIL"), "{stdout}");
    for want in [
        "  ok    logins answered 200: 20\n",
        "  ok    RateLimit-Policy: \"login\";q=5;w=60\n",
        "  ok    statuses: 401 401 401 401 401 429\n",
        "  ok    Retry-After: 12\n",
        "  ok    the right password is refused too: 429\n",
        "  ok    alice is unaffected: 200\n",
        "  ok    k1 admitted: 10\n",
        "  ok    k1 refused: 5\n",
        "  ok    k2 admitted: 200\n",
        "  ok    findings: 2\n",
        "\nall checks passed\n",
    ] {
        assert!(stdout.contains(want), "{want:?} in\n{stdout}");
    }
    assert_eq!(stdout.matches("\n  ok    ").count(), 18, "{stdout}");
}
