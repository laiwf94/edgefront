//! The edgefront binary. It wires the core runtime and the cache into a node
//! exactly as cachet's binary did: the same flags, exit codes, `CACHET_*`
//! environment variables and JSON log fields, under its own name. The golden
//! transcripts in tests/golden hold both binaries to that.
//!
//! `edgefront proxy` runs the HTTP/1.1 proxy ported from steer instead; see
//! the `proxy` module, and `edgefront check` lints a limiter config; see
//! the `check` module.

mod check;
mod cli;
mod proxy;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    cli::run(&args)
}
