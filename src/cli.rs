//! The command line shared by the `cachet` and `edgefront` binaries: the
//! flags, exit codes, environment variables and log setup are the same for
//! both, and only the program name in messages differs.

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

use crate::config::{Config, Secrets};

fn usage(program: &str) -> String {
    format!(
        "usage: {program} --config <file>\n       {program} --check-config <file>\n       {program} --version\n"
    )
}

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

/// Runs the binary named `program` with `args` (without the program name)
/// and returns its exit code: 0 on success, 2 for bad usage or config, 1
/// when the node cannot start.
pub fn main(program: &str, args: &[String]) -> ExitCode {
    let (check_only, path) = match args {
        [flag] if flag == "--version" => {
            println!("{program} {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        [flag, path] if flag == "--config" => (false, path.clone()),
        [flag, path] if flag == "--check-config" => (true, path.clone()),
        _ => {
            eprint!("{}", usage(program));
            return ExitCode::from(2);
        }
    };
    let config = match load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{program}: {e}");
            return ExitCode::from(2);
        }
    };
    let secrets = Secrets::from_env();
    if let Err(e) = config.validate_secrets(&secrets) {
        eprintln!("{program}: {e}");
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
            eprintln!("{program}: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async {
        let node = match crate::node::start(&config, &secrets).await {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(error = %e, "cannot start");
                return ExitCode::FAILURE;
            }
        };
        wait_for_signal().await;
        tracing::info!("shutting down");
        node.shutdown().await;
        ExitCode::SUCCESS
    })
}
