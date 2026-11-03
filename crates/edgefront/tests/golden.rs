//! The edgefront binary prints the same transcripts as the cachet binary it
//! replaces: see tests/golden/harness.rs. The reference cachet binary runs
//! the same cases in the root package's tests/golden_cli.rs.

#[path = "golden/harness.rs"]
mod harness;

use harness::{Bin, check};

static BIN: Bin = Bin {
    path: env!("CARGO_BIN_EXE_edgefront"),
    program: "edgefront",
    golden: concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden"),
};

#[test]
fn usage() {
    check(&BIN, "usage");
}

#[test]
fn check_config() {
    check(&BIN, "check_config");
}

#[cfg(target_os = "linux")]
#[test]
fn serve() {
    check(&BIN, "serve");
}

#[cfg(target_os = "linux")]
#[test]
fn quiet() {
    check(&BIN, "quiet");
}

#[test]
fn cannot_start() {
    check(&BIN, "cannot_start");
}
