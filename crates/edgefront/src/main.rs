//! The edgefront binary. It runs the same node, with the same flags, exit
//! codes and environment variables, as the `cachet` binary it replaces.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    cachet::cli::main("edgefront", &args)
}
