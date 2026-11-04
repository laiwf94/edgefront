//! `edgefront proxy`: the HTTP/1.1 proxy ported from steer, run from a
//! steer configuration file (JSON, or the same schema in TOML).
//!
//! ```text
//! edgefront proxy --config <file>
//! edgefront proxy --check-config <file>
//! ```
//!
//! Exit codes follow the rest of the binary: 0 on success, 2 for bad usage
//! or an unreadable or invalid file, 1 when the proxy cannot start. The log
//! is JSON on stdout, filtered by `EDGEFRONT_LOG` (default `info`); the
//! first line, `listening`, names the bound address in `proxy`, as steer's
//! did. SIGTERM or SIGINT stops accepting, lets requests in flight finish
//! for up to 30 seconds and exits.
//!
//! The admin listener and TLS termination are ported in later weeks; until
//! then `admin_listen` is validated but not served, and a file with a `tls`
//! block is refused rather than served in plain text.

use std::process::ExitCode;
use std::sync::Arc;

use edgefront_core::config::Config;
use edgefront_core::listener::{self, ServerLimits};
use edgefront_proxy::server::Proxy;
use tracing_subscriber::EnvFilter;

const USAGE: &str =
    "usage: edgefront proxy --config <file>\n       edgefront proxy --check-config <file>\n";

/// Binds a listen address as Go's `net.Listen` does: an empty host, as in
/// `:8080`, means every interface, both IPv6 and IPv4 where IPv6 is on.
async fn bind(listen: &str) -> std::io::Result<tokio::net::TcpListener> {
    match edgefront_core::config::go::split_host_port(listen) {
        Ok(("", port)) => match tokio::net::TcpListener::bind(format!("[::]:{port}")).await {
            Ok(s) => Ok(s),
            Err(_) => tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await,
        },
        _ => tokio::net::TcpListener::bind(listen).await,
    }
}

/// Runs `edgefront proxy` with the arguments after `proxy`.
pub fn run(args: &[String]) -> ExitCode {
    let (check_only, path) = match args {
        [flag, path] if flag == "--config" => (false, path.as_str()),
        [flag, path] if flag == "--check-config" => (true, path.as_str()),
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let config = match Config::load(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("edgefront: {path}: {e}");
            return ExitCode::from(2);
        }
    };
    if config.tls.is_some() {
        eprintln!("edgefront: {path}: tls is not supported by the proxy yet; remove the tls block");
        return ExitCode::from(2);
    }
    if check_only {
        println!("{path}: ok");
        return ExitCode::SUCCESS;
    }

    let filter =
        EnvFilter::try_from_env("EDGEFRONT_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
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
            eprintln!("edgefront: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async {
        let socket = match bind(&config.listen).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(listen = config.listen, error = %e, "cannot listen");
                return ExitCode::FAILURE;
            }
        };
        let addr = socket
            .local_addr()
            .map_or_else(|_| config.listen.clone(), |a| a.to_string());
        tracing::info!(
            proxy = addr,
            tls = false,
            clusters = config.clusters.len(),
            routes = config.routes.len(),
            "listening"
        );
        let proxy = Arc::new(Proxy::new(&config));
        let report = listener::serve(
            socket,
            proxy,
            ServerLimits::default(),
            crate::cli::wait_for_signal(),
        )
        .await;
        tracing::info!(
            accepted = report.accepted,
            forced = report.forced,
            "stopped"
        );
        ExitCode::SUCCESS
    })
}
