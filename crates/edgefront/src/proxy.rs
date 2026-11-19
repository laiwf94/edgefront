//! `edgefront proxy`: the HTTP/1.1 proxy ported from steer, run from a
//! steer configuration file (JSON, or the same schema in TOML), with
//! steer's admin API on `admin_listen`.
//!
//! ```text
//! edgefront proxy --config <file>
//! edgefront proxy --check-config <file>
//! ```
//!
//! Exit codes follow the rest of the binary: 0 on success, 2 for bad usage,
//! an unreadable or invalid file or bad settings, 1 when the proxy cannot
//! start. The log is JSON on stdout, filtered by `EDGEFRONT_LOG` (default
//! `info`); the first line, `listening`, names the bound addresses in
//! `proxy` and `admin`, as steer's did.
//!
//! Settings come from the environment, as steer's did:
//!
//! | variable | meaning |
//! |---|---|
//! | `EDGEFRONT_ADMIN_TOKEN` | bearer token for the admin API, at least 16 characters; required when `admin_listen` is not a loopback address |
//! | `EDGEFRONT_DRAIN_PERIOD` | how long `/readyz` reports draining before shutdown starts, a Go duration from `0s` to `10m` (default `5s`) |
//! | `EDGEFRONT_SHUTDOWN_TIMEOUT` | how long requests in flight get to finish after that, from `0s` to `1h` (default `30s`) |
//!
//! SIGHUP, or `POST /reload` on the admin listener, re-reads the file and
//! applies it with [`Proxy::reload`]: endpoints keep their health and
//! ejection state, and a file that does not validate leaves the running
//! configuration serving. SIGTERM or SIGINT drains: `/readyz` answers 503
//! so load balancers stop sending traffic and responses close their
//! connections; after the drain period both listeners stop accepting and
//! requests in flight have the shutdown timeout to finish.
//!
//! With a `tls` block the proxy listener terminates TLS (see
//! [`edgefront_core::tls`]): certificates are chosen by SNI, and a reload
//! re-reads them, so new handshakes get the new certificates while live
//! connections keep theirs. A reload cannot turn TLS on or off.
//!
//! Health checks and outlier detection run beside the listener from the
//! start and stop after it has drained; retries run on the request path
//! (see [`edgefront_proxy::server`]). The lifecycle itself is
//! [`edgefront_core::reload`].

use std::process::ExitCode;
use std::sync::Arc;

use edgefront_core::config::Config;
use edgefront_core::reload::{self, Settings, Signals};
use edgefront_core::tls::Snapshot;
use edgefront_proxy::server::Proxy;
use tracing_subscriber::EnvFilter;

const USAGE: &str =
    "usage: edgefront proxy --config <file>\n       edgefront proxy --check-config <file>\n";

/// The environment prefix of the binary's settings, as `STEER` was
/// steer's.
const PREFIX: &str = "EDGEFRONT";

/// Checks a file without serving it: the configuration, then every
/// certificate and key its `tls` block names.
fn check(path: &str) -> Result<(), String> {
    let config = Config::load(path).map_err(|e| format!("{path}: {e}"))?;
    if let Some(tls) = &config.tls {
        Snapshot::load(tls).map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(())
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
    if check_only {
        return match check(path) {
            Ok(()) => {
                println!("{path}: ok");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("edgefront: {e}");
                ExitCode::from(2)
            }
        };
    }
    let settings = match Settings::load(PREFIX, |k| std::env::var(k).ok()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("edgefront: {e}");
            return ExitCode::from(2);
        }
    };

    let filter =
        EnvFilter::try_from_env("EDGEFRONT_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_current_span(false)
        .init();

    // Loaded after the log is up, so certificate warnings are logged.
    let loaded = match reload::load(path, &settings, PREFIX) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("edgefront: {e}");
            return ExitCode::from(2);
        }
    };

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
        let proxy = Arc::new(Proxy::new(&loaded.config));
        let served = reload::run(
            loaded,
            path.to_owned(),
            settings,
            PREFIX,
            proxy,
            Signals::os(),
            |_| {},
        )
        .await;
        match served {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(listen = e.listen, error = %e.error, "cannot listen");
                ExitCode::FAILURE
            }
        }
    })
}
