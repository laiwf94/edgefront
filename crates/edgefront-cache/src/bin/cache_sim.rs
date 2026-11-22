//! Replays a cache trace through eviction policies at several cache sizes;
//! see `cache_sim --help` and `edgefront_cache::sim`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    edgefront_cache::sim::cli::main(&args, &mut std::io::stdout().lock(), &mut std::io::stderr())
}
