//! End to end: examples/limiter_config.sh against this build's edgefront
//! binary. The script loads pacer's example pacerd.conf and the new TOML
//! config with daily quotas in examples/limiter, converts one to the
//! other and checks each refusal and lint by its line; this test runs it
//! as CI's demo job does and checks that every step reported.

use std::process::Command;

#[test]
fn the_limiter_config_demo_passes_every_step() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let out = Command::new("bash")
        .arg(format!("{root}/examples/limiter_config.sh"))
        .env("EDGEFRONT_BIN", env!("CARGO_BIN_EXE_edgefront"))
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert_eq!(stdout.matches("\n== ").count(), 7, "{stdout}");
    for want in [
        "   ok: examples/limiter/pacerd.conf: ok\n",
        "   ok: converted.toml prints back byte for byte and lints clean\n",
        "   ok: examples/limiter/limiter.toml: ok\n",
        "   ok: per_ip_conn_quota = \"10000/day\"\n",
        "   ok: listener web is the same as in the converted pacerd.conf\n",
        "warning[quota-unreachable]: listener api",
        "\"25/min\" is a rate, not a quota",
        "\"1500/6h\" is a quota",
        "   ok: line 18: refused as pacerd refuses it\n",
        "   ok: line 28: table-below-max-conns\n",
        "\nall steps passed\n",
    ] {
        assert!(stdout.contains(want), "{want:?} in\n{stdout}");
    }
}

/// examples/limiter/pacerd.conf is pacer's own example, kept beside the
/// new config so the demo outlives legacy/pacer; while legacy/pacer is in
/// the tree, the two must not drift apart.
#[test]
fn the_example_pacerd_conf_reads_as_pacers() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let ours = edgefront_limiter::config::read_pacerd(std::path::Path::new(&format!(
        "{root}/examples/limiter/pacerd.conf"
    )))
    .unwrap();
    let pacers = edgefront_limiter::config::read_pacerd(std::path::Path::new(&format!(
        "{root}/legacy/pacer/examples/pacerd.conf"
    )))
    .unwrap();
    assert_eq!(ours, pacers);
}
