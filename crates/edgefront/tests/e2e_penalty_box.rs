//! End to end: examples/penalty_box.sh's first two steps against this
//! build's binary: a client boxed for staying over its limit, released on
//! time and banned on its second offence, connections closed after their
//! request quota, and a ban through the `edgefront limits` admin API that
//! survives a reload and expires on time. The third step, overload mode,
//! is edgefront-bench's to run; CI's demo job runs the whole script.

use std::net::TcpListener;
use std::process::Command;

/// The lowest port the kernel hands out for `:0` and outgoing
/// connections.
fn ephemeral_low() -> u16 {
    std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
        .ok()
        .and_then(|t| t.split_whitespace().next()?.parse().ok())
        .unwrap_or(32768)
}

/// The first of five consecutive ports that are free now, below the
/// ephemeral range (#259): a port the kernel never assigns on its own, so
/// another test binding `:0` or connecting out cannot take one between
/// this check and the script binding it. Above 20000, clear of the demos'
/// fixed defaults, and from an offset by process id so two runs at once
/// start apart.
fn free_ports() -> u16 {
    let low = 20_000u32;
    let high = u32::from(ephemeral_low()).max(low + 100) - 5;
    let span = (high - low) / 5;
    let start = std::process::id() % span;
    for k in 0..span {
        let p = (low + ((start + k) % span) * 5) as u16;
        if (0..5).all(|i| TcpListener::bind(("127.0.0.1", p + i)).is_ok()) {
            return p;
        }
    }
    panic!("no five consecutive free ports below the ephemeral range");
}

#[test]
fn the_ports_are_free_and_below_the_ephemeral_range() {
    let p = free_ports();
    assert!(
        p >= 20_000 && u32::from(p) + 4 < u32::from(ephemeral_low()),
        "{p}"
    );
    for i in 0..5 {
        TcpListener::bind(("127.0.0.1", p + i)).unwrap();
    }
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
