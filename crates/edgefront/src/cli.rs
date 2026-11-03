//! The command line: `--config`, `--check-config` and `--version`.
//!
//! Exit codes are 0 on success, 2 for bad usage, an unreadable or invalid
//! config or missing secrets, and 1 when the node cannot start. The log is
//! JSON on stdout, filtered by `CACHET_LOG` (default `info`); secrets come
//! from `CACHET_ADMIN_TOKEN` and `CACHET_CLUSTER_SECRET`.

use std::process::ExitCode;

use cachet::config::{Config, Secrets};
use tracing_subscriber::EnvFilter;

const PROGRAM: &str = "edgefront";

const USAGE: &str = "usage: edgefront --config <file>\n       edgefront --check-config <file>\n       edgefront --version\n";

/// The target of the events this module logs. cachet's binary logged them
/// under its crate name, so `CACHET_LOG` directives such as `cachet=warn`
/// and log pipelines that match on the target keep working unchanged.
const LOG_TARGET: &str = "cachet";

fn load(path: &str) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    Config::parse(&text).map_err(|e| format!("{path}: {e}"))
}

async fn wait_for_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => {
                    let _ = ctrl_c.await;
                    return;
                }
            };
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}

/// Runs the command line `args` (without the program name) and returns the
/// exit code.
pub fn run(args: &[String]) -> ExitCode {
    let (check_only, path) = match args {
        [flag] if flag == "--version" => {
            println!("{PROGRAM} {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        [flag, path] if flag == "--config" => (false, path.as_str()),
        [flag, path] if flag == "--check-config" => (true, path.as_str()),
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let config = match load(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{PROGRAM}: {e}");
            return ExitCode::from(2);
        }
    };
    let secrets = Secrets::from_env();
    if let Err(e) = config.validate_secrets(&secrets) {
        eprintln!("{PROGRAM}: {e}");
        return ExitCode::from(2);
    }
    if check_only {
        println!("{path}: ok");
        return ExitCode::SUCCESS;
    }

    let filter = EnvFilter::try_from_env("CACHET_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_current_span(false)
        .init();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{PROGRAM}: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async {
        let node = match cachet::node::start(&config, &secrets).await {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(target: LOG_TARGET, error = %e, "cannot start");
                return ExitCode::FAILURE;
            }
        };
        wait_for_signal().await;
        tracing::info!(target: LOG_TARGET, "shutting down");
        node.shutdown().await;
        ExitCode::SUCCESS
    })
}
