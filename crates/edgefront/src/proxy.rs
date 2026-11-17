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
//! Health checks and outlier detection run beside the listener from the
//! start and stop after it has drained; retries run on the request path
//! (see [`edgefront_proxy::server`]). TLS termination is ported in a later
//! week; until then a file with a `tls` block is refused rather than
//! served in plain text.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use edgefront_core::admin::{self, Api, Control, ProxyState};
use edgefront_core::config::{Config, go};
use edgefront_core::listener::{self, ServerLimits};
use edgefront_proxy::server::Proxy;
use tracing_subscriber::EnvFilter;

const USAGE: &str =
    "usage: edgefront proxy --config <file>\n       edgefront proxy --check-config <file>\n";

/// The settings steer read from its environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Settings {
    pub admin_token: String,
    pub drain_period: Duration,
    pub shutdown_timeout: Duration,
}

impl Settings {
    /// Reads the settings through `getenv`, with steer's defaults and
    /// bounds.
    pub(crate) fn load(getenv: impl Fn(&str) -> Option<String>) -> Result<Settings, String> {
        let admin_token = getenv("EDGEFRONT_ADMIN_TOKEN").unwrap_or_default();
        if !admin_token.is_empty() && admin_token.len() < admin::MIN_TOKEN_LENGTH {
            return Err(format!(
                "EDGEFRONT_ADMIN_TOKEN must be at least {} characters",
                admin::MIN_TOKEN_LENGTH
            ));
        }
        let duration = |name: &str, default: Duration, max: Duration| {
            let Some(text) = getenv(name).filter(|v| !v.is_empty()) else {
                return Ok(default);
            };
            match go::parse_duration(&text) {
                Some(n) if n >= 0 && n as u128 <= max.as_nanos() => {
                    Ok(Duration::from_nanos(n as u64))
                }
                _ => Err(format!(
                    "{name} must be a duration between 0s and {}",
                    go::format_duration(max.as_nanos() as i64)
                )),
            }
        };
        Ok(Settings {
            admin_token,
            drain_period: duration(
                "EDGEFRONT_DRAIN_PERIOD",
                Duration::from_secs(5),
                Duration::from_secs(600),
            )?,
            shutdown_timeout: duration(
                "EDGEFRONT_SHUTDOWN_TIMEOUT",
                Duration::from_secs(30),
                Duration::from_secs(3600),
            )?,
        })
    }
}

/// Whether a listen address only accepts local connections: `localhost`
/// or a loopback IP, as steer's `loopback` decided.
pub(crate) fn loopback(addr: &str) -> bool {
    let Ok((host, _)) = go::split_host_port(addr) else {
        return false;
    };
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Loads and checks the file as steer's `loadConfig` did: a valid file,
/// no `tls` block yet, and a token unless the admin API is on loopback.
fn load(path: &str, settings: &Settings) -> Result<Config, String> {
    let config = Config::load(path).map_err(|e| format!("{path}: {e}"))?;
    if config.tls.is_some() {
        return Err(format!(
            "{path}: tls is not supported by the proxy yet; remove the tls block"
        ));
    }
    if settings.admin_token.is_empty() && !loopback(&config.admin_listen) {
        return Err(format!(
            "admin_listen {} is not a loopback address; set EDGEFRONT_ADMIN_TOKEN to expose the admin API",
            config.admin_listen
        ));
    }
    Ok(config)
}

/// Binds a listen address as Go's `net.Listen` does: an empty host, as in
/// `:8080`, means every interface, both IPv6 and IPv4 where IPv6 is on.
async fn bind(listen: &str) -> std::io::Result<tokio::net::TcpListener> {
    match go::split_host_port(listen) {
        Ok(("", port)) => match tokio::net::TcpListener::bind(format!("[::]:{port}")).await {
            Ok(s) => Ok(s),
            Err(_) => tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await,
        },
        _ => tokio::net::TcpListener::bind(listen).await,
    }
}

/// What the admin API administers: the running proxy and the file it
/// was loaded from.
struct Running {
    proxy: Arc<Proxy>,
    path: String,
    settings: Settings,
    /// One reload at a time, from the admin API or SIGHUP.
    reloading: tokio::sync::Mutex<()>,
}

impl Running {
    /// Re-reads the file and applies it, as steer's `reload` did.
    async fn reload(&self) -> Result<u64, String> {
        let _held = self.reloading.lock().await;
        let result = match load(&self.path, &self.settings) {
            Ok(config) => self.proxy.reload(&config),
            Err(e) => {
                self.proxy.record_failed_reload();
                Err(e)
            }
        };
        if let Err(e) = &result {
            tracing::warn!(
                error = e.as_str(),
                "configuration rejected; the previous configuration keeps serving"
            );
        }
        result
    }
}

impl Control for Running {
    fn ready(&self) -> bool {
        !self.proxy.draining()
    }

    fn state(&self) -> ProxyState {
        self.proxy.state()
    }

    async fn reload(&self) -> Result<u64, String> {
        Running::reload(self).await
    }

    fn metrics(&self, out: &mut String) {
        self.proxy.metrics().registry().write(out);
    }
}

/// steer's admin listener: 5 seconds for the head, 60 seconds idle and
/// 16 KiB of head.
fn admin_limits(drain: Duration) -> ServerLimits {
    ServerLimits {
        header_read_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(60),
        max_header_bytes: 16 << 10,
        drain_timeout: drain,
        ..ServerLimits::default()
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
    if check_only {
        return match Config::load(path) {
            Ok(c) if c.tls.is_some() => {
                eprintln!(
                    "edgefront: {path}: tls is not supported by the proxy yet; remove the tls block"
                );
                ExitCode::from(2)
            }
            Ok(_) => {
                println!("{path}: ok");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("edgefront: {path}: {e}");
                ExitCode::from(2)
            }
        };
    }
    let settings = match Settings::load(|k| std::env::var(k).ok()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("edgefront: {e}");
            return ExitCode::from(2);
        }
    };
    let config = match load(path, &settings) {
        Ok(c) => c,
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
    runtime.block_on(serve(config, path.to_owned(), settings))
}

async fn serve(config: Config, path: String, settings: Settings) -> ExitCode {
    let socket = match bind(&config.listen).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(listen = config.listen, error = %e, "cannot listen");
            return ExitCode::FAILURE;
        }
    };
    let admin_socket = match bind(&config.admin_listen).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(listen = config.admin_listen, error = %e, "cannot listen");
            return ExitCode::FAILURE;
        }
    };
    let proxy = Arc::new(Proxy::new(&config));
    let running = Arc::new(Running {
        proxy: Arc::clone(&proxy),
        path,
        settings: settings.clone(),
        reloading: tokio::sync::Mutex::new(()),
    });
    let api = Arc::new(
        Api::new(Arc::clone(&running), &settings.admin_token)
            .expect("Settings::load refuses a token Api::new would"),
    );
    let addr = |s: &tokio::net::TcpListener, fallback: &str| {
        s.local_addr()
            .map_or_else(|_| fallback.to_owned(), |a| a.to_string())
    };
    tracing::info!(
        proxy = addr(&socket, &config.listen),
        tls = false,
        admin = addr(&admin_socket, &config.admin_listen),
        admin_auth = api.authenticates(),
        clusters = config.clusters.len(),
        routes = config.routes.len(),
        "listening"
    );

    let (stop_checks, checks_stopped) = tokio::sync::oneshot::channel::<()>();
    let checks = tokio::spawn({
        let proxy = Arc::clone(&proxy);
        async move {
            proxy
                .run_checks(async move {
                    let _ = checks_stopped.await;
                })
                .await;
        }
    });
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let until_stopped = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|s| *s).await;
    };
    let limits = ServerLimits {
        drain_timeout: settings.shutdown_timeout,
        ..ServerLimits::default()
    };
    let proxy_server = tokio::spawn(listener::serve(
        socket,
        Arc::clone(&proxy),
        limits,
        until_stopped(stopped.clone()),
    ));
    let admin_server = tokio::spawn(listener::serve(
        admin_socket,
        api,
        admin_limits(settings.shutdown_timeout),
        until_stopped(stopped),
    ));

    wait_for_stop(&running).await;
    proxy.drain();
    tracing::info!(
        drain_period_ms = settings.drain_period.as_millis() as u64,
        "draining"
    );
    tokio::time::sleep(settings.drain_period).await;
    tracing::info!(
        timeout_ms = settings.shutdown_timeout.as_millis() as u64,
        "shutting down"
    );
    stop.send_replace(true);
    let report = proxy_server.await.unwrap_or_default();
    let admin_report = admin_server.await.unwrap_or_default();
    for (name, forced) in [("proxy", report.forced), ("admin", admin_report.forced)] {
        if forced > 0 {
            tracing::warn!(
                listener = name,
                forced,
                "shutdown timeout reached; closing remaining connections"
            );
        }
    }
    drop(stop_checks);
    let _ = checks.await;
    tracing::info!(
        accepted = report.accepted,
        forced = report.forced,
        "stopped"
    );
    ExitCode::SUCCESS
}

/// Serves SIGHUP as a reload until SIGTERM or SIGINT arrives.
async fn wait_for_stop(running: &Running) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut hup) = signal(SignalKind::hangup()) {
            let stop = crate::cli::wait_for_signal();
            tokio::pin!(stop);
            loop {
                tokio::select! {
                    () = &mut stop => return,
                    _ = hup.recv() => {
                        if let Ok(version) = running.reload().await {
                            tracing::info!(version, "reloaded on SIGHUP");
                        }
                    }
                }
            }
        }
    }
    crate::cli::wait_for_signal().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn settings_default_as_steer() {
        let s = Settings::load(env(&[])).unwrap();
        assert_eq!(
            s,
            Settings {
                admin_token: String::new(),
                drain_period: Duration::from_secs(5),
                shutdown_timeout: Duration::from_secs(30),
            }
        );
        let s = Settings::load(env(&[
            ("EDGEFRONT_ADMIN_TOKEN", "0123456789abcdef"),
            ("EDGEFRONT_DRAIN_PERIOD", "0s"),
            ("EDGEFRONT_SHUTDOWN_TIMEOUT", "1h"),
        ]))
        .unwrap();
        assert_eq!(s.admin_token, "0123456789abcdef");
        assert_eq!(s.drain_period, Duration::ZERO);
        assert_eq!(s.shutdown_timeout, Duration::from_secs(3600));
        // An empty value is the default, as Go's getenv returns "".
        let s = Settings::load(env(&[("EDGEFRONT_DRAIN_PERIOD", "")])).unwrap();
        assert_eq!(s.drain_period, Duration::from_secs(5));
    }

    #[test]
    fn settings_refuse_what_steer_refused() {
        let err = |pairs: &[(&str, &str)]| Settings::load(env(pairs)).unwrap_err();
        assert_eq!(
            err(&[("EDGEFRONT_ADMIN_TOKEN", "short")]),
            "EDGEFRONT_ADMIN_TOKEN must be at least 16 characters"
        );
        for bad in ["-1s", "11m", "soon", "5"] {
            assert_eq!(
                err(&[("EDGEFRONT_DRAIN_PERIOD", bad)]),
                "EDGEFRONT_DRAIN_PERIOD must be a duration between 0s and 10m0s",
                "{bad}"
            );
        }
        assert_eq!(
            err(&[("EDGEFRONT_SHUTDOWN_TIMEOUT", "1h0m1s")]),
            "EDGEFRONT_SHUTDOWN_TIMEOUT must be a duration between 0s and 1h0m0s"
        );
    }

    #[test]
    fn loopback_matches_steer() {
        for addr in ["127.0.0.1:9901", "localhost:1", "[::1]:9901", "127.9.9.9:0"] {
            assert!(loopback(addr), "{addr}");
        }
        for addr in [
            ":9901",
            "0.0.0.0:9901",
            "[::]:1",
            "10.0.0.1:1",
            "example:1",
            "nonsense",
        ] {
            assert!(!loopback(addr), "{addr}");
        }
    }
}
