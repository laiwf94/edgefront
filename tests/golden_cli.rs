//! The reference cachet binary against the golden transcripts in
//! crates/edgefront/tests/golden, which the edgefront binary must match too.

#[path = "../crates/edgefront/tests/golden/harness.rs"]
mod harness;

use harness::{Bin, check};

static BIN: Bin = Bin {
    path: env!("CARGO_BIN_EXE_cachet"),
    program: "cachet",
    golden: concat!(env!("CARGO_MANIFEST_DIR"), "/crates/edgefront/tests/golden"),
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
