//! `edgefront limits`: pacerd's TCP proxy with per-client and per-listener
//! limits, ported from `src/pacerd`. It reads pacerd's config file (or the
//! TOML limiter section) and behaves as pacerd does: the same admission
//! limits, byte shaping, slow-client cut-off, descriptor-exhaustion
//! handling, drain on SIGTERM and reload on SIGHUP, with the same log
//! lines. The admin socket speaks HTTP ([`admin`]), and `edgefront ctl
//! limits` is its client.
//!
//! ```text
//! edgefront limits -c CONFIG       run
//! edgefront limits -c CONFIG -t    check the config and exit
//! ```
//!
//! Exit codes are pacerd's: 0 after a drain, 1 when the daemon cannot
//! start, 2 for bad usage or a bad config.

pub mod admin;
mod conn;
mod daemon;
mod log;
mod stats;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: edgefront limits -c CONFIG [-t]\n  -c CONFIG  config file (pacerd.conf, or .toml)\n  -t         check the config and exit\n";

/// Runs `edgefront limits` with the arguments after `limits`.
pub fn run(args: &[String]) -> ExitCode {
    let (path, check) = match args {
        [c, p] if c == "-c" => (PathBuf::from(p), false),
        [c, p, t] | [t, c, p] if c == "-c" && t == "-t" => (PathBuf::from(p), true),
        [h] if h == "-h" || h == "--help" => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    if check {
        return match daemon::load(&path) {
            Ok(cfg) => {
                let n = cfg.listeners.len();
                println!(
                    "edgefront: {} ok, {n} listener{}",
                    path.display(),
                    if n == 1 { "" } else { "s" }
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("edgefront: {}: {e}", path.display());
                ExitCode::from(2)
            }
        };
    }
    daemon::run(path)
}
