//! The edgefront binary. It wires the core runtime and the cache into a node
//! exactly as cachet's binary did: the same flags, exit codes, `CACHET_*`
//! environment variables and JSON log fields, under its own name. The golden
//! transcripts in tests/golden hold both binaries to that.

mod cli;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    cli::run(&args)
}
