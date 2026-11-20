//! A proxy process's lifecycle, ported from steer's `cmd/steer/main.go`:
//! settings from the environment, the file and its certificates loaded and
//! checked, the proxy and admin listeners, reloads on SIGHUP or
//! `POST /reload`, and the drain on SIGTERM or SIGINT.
//!
//! [`run`] serves a [`Service`] (the proxy, through a trait so this crate
//! does not depend on it) until a stop signal, then drains:
//!
//! 1. the service drains: `/readyz` answers 503 and every response closes
//!    its connection, so load balancers and clients move elsewhere;
//! 2. the drain period passes;
//! 3. both listeners stop accepting, and requests in flight have the
//!    shutdown timeout to finish, after which what is left is closed;
//! 4. the service's background work (health checks and outlier ticks)
//!    stops.
//!
//! A reload re-reads the file, certificates and keys included, and is
//! applied whole or not at all: a file that does not load, a `tls` block
//! that appears or disappears (the listener would have to change), a
//! certificate that cannot be read, or a change the service refuses
//! (`listen`, `admin_listen`) leaves the running configuration serving and
//! counts as a failed reload. Reloads are serialised, so SIGHUP and
//! `POST /reload` never interleave. A reload's new certificates are
//! swapped into the [`Store`] only after the service has taken the rest
//! of the file, and only new handshakes see them.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::admin::{self, Api, Control, ProxyState};
use crate::clock::MonotonicClock;
use crate::config::{Config, go};
use crate::listener::{self, Handler, ServeReport, ServerLimits};
use crate::telemetry::metrics::TlsMetrics;
use crate::tls::ocsp::{Stapler, Staples, Wall};
use crate::tls::{Acceptor, Shared, Snapshot, Store, TicketKeys, tickets};

/// The settings steer read from its environment, under a prefix
/// (`EDGEFRONT` for the binary, as `STEER` was steer's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub admin_token: String,
    /// How long `/readyz` reports draining before the listeners close.
    pub drain_period: Duration,
    /// How long requests in flight get to finish after that.
    pub shutdown_timeout: Duration,
}

impl Settings {
    /// Reads `<prefix>_ADMIN_TOKEN`, `<prefix>_DRAIN_PERIOD` (default 5 s,
    /// at most 10 minutes) and `<prefix>_SHUTDOWN_TIMEOUT` (default 30 s,
    /// at most an hour) through `getenv`, with steer's bounds and
    /// messages. An empty value is the default, as Go's `Getenv` returned
    /// `""` for unset.
    pub fn load(prefix: &str, getenv: impl Fn(&str) -> Option<String>) -> Result<Settings, String> {
        let token_var = format!("{prefix}_ADMIN_TOKEN");
        let admin_token = getenv(&token_var).unwrap_or_default();
        if !admin_token.is_empty() && admin_token.len() < admin::MIN_TOKEN_LENGTH {
            return Err(format!(
                "{token_var} must be at least {} characters",
                admin::MIN_TOKEN_LENGTH
            ));
        }
        let duration = |suffix: &str, default: Duration, max: Duration| {
            let name = format!("{prefix}_{suffix}");
            let Some(text) = getenv(&name).filter(|v| !v.is_empty()) else {
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
                "DRAIN_PERIOD",
                Duration::from_secs(5),
                Duration::from_secs(600),
            )?,
            shutdown_timeout: duration(
                "SHUTDOWN_TIMEOUT",
                Duration::from_secs(30),
                Duration::from_secs(3600),
            )?,
        })
    }
}

/// Whether a listen address only accepts local connections: `localhost`
/// or a loopback IP, as steer's `loopback` decided.
pub fn loopback(addr: &str) -> bool {
    let Ok((host, _)) = go::split_host_port(addr) else {
        return false;
    };
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// A configuration file as loaded: the configuration, and its `tls`
/// block's certificates when it has one.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub tls: Option<Arc<Snapshot>>,
    /// What every later snapshot shares with this one: the session ticket
    /// keys, rotated hourly on the monotonic clock.
    pub shared: Shared,
}

/// Loads and checks `path` as steer's `loadConfig` and `loadTLS` did: a
/// valid file, a token unless the admin API is on loopback, and every
/// certificate and key readable and matched, the clusters' upstream TLS
/// files included. Problems with certificates
/// that still serve (expired, not yet valid, a name not covered) are
/// logged as `tls certificate` warnings.
pub fn load(path: &str, settings: &Settings, prefix: &str) -> Result<Loaded, String> {
    let tickets = TicketKeys::new(MonotonicClock::shared(), tickets::DEFAULT_PERIOD)
        .map_err(|e| format!("cannot make session ticket keys: {e}"))?;
    let shared = Shared {
        tickets: Some(tickets),
        staples: Some(Arc::new(Staples::default())),
        alpn_challenges: None,
    };
    load_with(path, settings, prefix, shared)
}

/// Like [`load`], with the snapshot sharing `shared` (a reload passes the
/// running listener's).
pub fn load_with(
    path: &str,
    settings: &Settings,
    prefix: &str,
    shared: Shared,
) -> Result<Loaded, String> {
    let config = Config::load(path).map_err(|e| format!("{path}: {e}"))?;
    if settings.admin_token.is_empty() && !loopback(&config.admin_listen) {
        return Err(format!(
            "admin_listen {} is not a loopback address; set {prefix}_ADMIN_TOKEN to expose the admin API",
            config.admin_listen
        ));
    }
    for (i, c) in config.clusters.iter().enumerate() {
        if let Some(t) = &c.tls {
            crate::http1::TlsTo::load(t).map_err(|e| format!("{path}: clusters[{i}].tls: {e}"))?;
        }
    }
    let tls = match &config.tls {
        None => None,
        Some(t) => {
            let snapshot = Snapshot::load_with(t, &shared).map_err(|e| format!("{path}: {e}"))?;
            for problem in snapshot.warnings(SystemTime::now()) {
                tracing::warn!(problem = problem.as_str(), "tls certificate");
            }
            Some(Arc::new(snapshot))
        }
    };
    Ok(Loaded {
        config,
        tls,
        shared,
    })
}

/// What [`run`] serves and reloads: the proxy, seen through what its
/// lifecycle needs.
pub trait Service: Handler {
    /// Applies a reloaded configuration, returning the new version, or
    /// refuses it (and counts the refusal).
    fn reload(&self, config: &Config) -> Result<u64, String>;
    /// Counts a reload refused before it reached [`Service::reload`].
    fn record_failed_reload(&self);
    /// Starts the drain: not ready, and every response closes its
    /// connection.
    fn drain(&self);
    /// Whether [`Service::drain`] has been called.
    fn draining(&self) -> bool;
    /// What `GET /clusters` shows.
    fn state(&self) -> ProxyState;
    /// Writes the Prometheus exposition for `GET /metrics`.
    fn write_metrics(&self, out: &mut String);
    /// Registers the TLS listener's series; `in_progress` is read on each
    /// scrape.
    fn tls_metrics(&self, in_progress: Box<dyn Fn() -> u64 + Send + Sync>) -> TlsMetrics;
    /// Work that runs beside the listeners until `stop` resolves, such as
    /// health checks.
    fn background(
        self: Arc<Self>,
        stop: Pin<Box<dyn Future<Output = ()> + Send>>,
    ) -> impl Future<Output = ()> + Send;
}

/// The running process as the admin API and SIGHUP see it: the service,
/// the file it came from and the certificate store.
pub struct Lifecycle<S> {
    service: Arc<S>,
    path: String,
    settings: Settings,
    prefix: String,
    tls: Option<Arc<Store>>,
    shared: Shared,
    /// One reload at a time, from the admin API or SIGHUP.
    reloading: tokio::sync::Mutex<()>,
}

impl<S: Service> Lifecycle<S> {
    /// Re-reads the file and applies it, as steer's `reload` did.
    pub async fn reload(&self) -> Result<u64, String> {
        let _held = self.reloading.lock().await;
        let result = match self.load_next() {
            Ok(next) => self.service.reload(&next.config).inspect(|_| {
                // Handshakes from now on use the new certificates;
                // established connections keep the ones they negotiated.
                if let (Some(store), Some(snapshot)) = (&self.tls, next.tls) {
                    store.swap(snapshot);
                }
            }),
            Err(e) => {
                self.service.record_failed_reload();
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

    fn load_next(&self) -> Result<Loaded, String> {
        // The TLS question is settled before any certificate is read.
        let config = Config::load(&self.path).map_err(|e| format!("{}: {e}", self.path))?;
        if config.tls.is_some() != self.tls.is_some() {
            return Err("tls cannot be turned on or off by a reload; restart edgefront".to_owned());
        }
        drop(config);
        load_with(
            &self.path,
            &self.settings,
            &self.prefix,
            self.shared.clone(),
        )
    }
}

impl<S: Service> Control for Lifecycle<S> {
    fn ready(&self) -> bool {
        !self.service.draining()
    }

    fn state(&self) -> ProxyState {
        self.service.state()
    }

    async fn reload(&self) -> Result<u64, String> {
        Lifecycle::reload(self).await
    }

    fn metrics(&self, out: &mut String) {
        self.service.write_metrics(out);
    }
}

/// Where reload and stop requests come from.
pub struct Signals {
    hup: mpsc::UnboundedReceiver<()>,
    stop: Pin<Box<dyn Future<Output = ()> + Send>>,
}

/// The sending side of [`Signals::channel`], for tests and embedders.
#[derive(Debug, Clone)]
pub struct SignalSender {
    hup: mpsc::UnboundedSender<()>,
    stop: tokio::sync::watch::Sender<bool>,
}

impl SignalSender {
    /// Asks for a reload, as SIGHUP does.
    pub fn hup(&self) {
        let _ = self.hup.send(());
    }

    /// Asks for the drain and shutdown, as SIGTERM does.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }
}

impl Signals {
    /// The process's signals: SIGHUP reloads, SIGTERM or SIGINT stops.
    /// Must be called inside a tokio runtime.
    pub fn os() -> Signals {
        let (hup_tx, hup) = mpsc::unbounded_channel();
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut s) = signal(SignalKind::hangup()) {
                tokio::spawn(async move {
                    while s.recv().await.is_some() {
                        if hup_tx.send(()).is_err() {
                            return;
                        }
                    }
                });
            }
        }
        #[cfg(not(unix))]
        drop(hup_tx);
        Signals {
            hup,
            stop: Box::pin(stop_signal()),
        }
    }

    /// Signals driven by the returned sender. Dropping the sender stops.
    pub fn channel() -> (SignalSender, Signals) {
        let (hup_tx, hup) = mpsc::unbounded_channel();
        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
        let stop = async move {
            let _ = stop_rx.wait_for(|s| *s).await;
        };
        (
            SignalSender {
                hup: hup_tx,
                stop: stop_tx,
            },
            Signals {
                hup,
                stop: Box::pin(stop),
            },
        )
    }
}

/// Resolves on SIGTERM or SIGINT.
pub async fn stop_signal() {
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

/// Binds a listen address as Go's `net.Listen` does: an empty host, as in
/// `:8080`, means every interface, both IPv6 and IPv4 where IPv6 is on.
pub async fn bind(listen: &str) -> std::io::Result<TcpListener> {
    match go::split_host_port(listen) {
        Ok(("", port)) => match TcpListener::bind(format!("[::]:{port}")).await {
            Ok(s) => Ok(s),
            Err(_) => TcpListener::bind(format!("0.0.0.0:{port}")).await,
        },
        _ => TcpListener::bind(listen).await,
    }
}

/// steer's admin listener: 5 seconds for the head, 60 seconds idle and
/// 16 KiB of head.
pub fn admin_limits(drain: Duration) -> ServerLimits {
    ServerLimits {
        header_read_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(60),
        max_header_bytes: 16 << 10,
        drain_timeout: drain,
        ..ServerLimits::default()
    }
}

/// The addresses [`run`] listens on, given to its `ready` callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bound {
    pub proxy: SocketAddr,
    pub admin: SocketAddr,
}

/// A listen address that could not be bound.
#[derive(Debug)]
pub struct ListenError {
    pub listen: String,
    pub error: std::io::Error,
}

impl std::fmt::Display for ListenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot listen on {}: {}", self.listen, self.error)
    }
}

impl std::error::Error for ListenError {}

/// What the two listeners did, once [`run`] has stopped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stopped {
    pub proxy: ServeReport,
    pub admin: ServeReport,
}

/// Serves `service` on `loaded`'s listeners, with the admin API on
/// `admin_listen`, until `signals` says stop, then drains as the module
/// documentation describes. `path` is re-read on every reload. `ready`
/// gets the bound addresses once both listeners are up.
pub async fn run<S: Service>(
    loaded: Loaded,
    path: String,
    settings: Settings,
    prefix: &str,
    service: Arc<S>,
    mut signals: Signals,
    ready: impl FnOnce(Bound),
) -> Result<Stopped, ListenError> {
    let config = loaded.config;
    let socket = bind(&config.listen).await.map_err(|error| ListenError {
        listen: config.listen.clone(),
        error,
    })?;
    let admin_socket = bind(&config.admin_listen)
        .await
        .map_err(|error| ListenError {
            listen: config.admin_listen.clone(),
            error,
        })?;
    let store = loaded.tls.map(Store::new);
    let lifecycle = Arc::new(Lifecycle {
        service: Arc::clone(&service),
        path,
        settings: settings.clone(),
        prefix: prefix.to_owned(),
        tls: store.clone(),
        shared: loaded.shared,
        reloading: tokio::sync::Mutex::new(()),
    });
    let api = Arc::new(
        Api::new(Arc::clone(&lifecycle), &settings.admin_token)
            .expect("Settings::load refuses a token Api::new would"),
    );
    let local = |s: &TcpListener| s.local_addr().expect("a bound socket has an address");
    let bound = Bound {
        proxy: local(&socket),
        admin: local(&admin_socket),
    };
    tracing::info!(
        proxy = bound.proxy.to_string(),
        tls = store.is_some(),
        admin = bound.admin.to_string(),
        admin_auth = api.authenticates(),
        clusters = config.clusters.len(),
        routes = config.routes.len(),
        "listening"
    );

    let (stop_background, background_stopped) = tokio::sync::oneshot::channel::<()>();
    let background = tokio::spawn(Arc::clone(&service).background(Box::pin(async move {
        let _ = background_stopped.await;
    })));
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let until_stopped = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|s| *s).await;
    };
    let limits = ServerLimits {
        drain_timeout: settings.shutdown_timeout,
        ..ServerLimits::default()
    };
    let proxy_server = match store {
        None => tokio::spawn(listener::serve(
            socket,
            Arc::clone(&service),
            limits,
            until_stopped(stopped.clone()),
        )),
        Some(store) => {
            if let (Some(tls), Some(staples)) = (&config.tls, &lifecycle.shared.staples)
                && tls.ocsp.stapling
            {
                let stapler = Stapler::new(
                    Arc::clone(&store),
                    Arc::clone(staples),
                    Wall::system(),
                    tls.ocsp.responder.clone(),
                );
                let mut stop = stopped.clone();
                tokio::spawn(stapler.run(MonotonicClock::shared(), async move {
                    let _ = stop.wait_for(|s| *s).await;
                }));
            }
            let acceptor = Acceptor::new(store, None);
            let in_progress = Arc::downgrade(&acceptor);
            let metrics = service.tls_metrics(Box::new(move || {
                in_progress.upgrade().map_or(0, |a| a.in_progress() as u64)
            }));
            acceptor.set_on_handshake(Arc::new(move |result, took| {
                metrics.handshake(result, took);
            }));
            tokio::spawn(listener::serve_tls(
                socket,
                acceptor,
                Arc::clone(&service),
                limits,
                until_stopped(stopped.clone()),
            ))
        }
    };
    let admin_server = tokio::spawn(listener::serve(
        admin_socket,
        api,
        admin_limits(settings.shutdown_timeout),
        until_stopped(stopped),
    ));
    ready(bound);

    loop {
        tokio::select! {
            () = &mut signals.stop => break,
            Some(()) = signals.hup.recv() => {
                if let Ok(version) = lifecycle.reload().await {
                    tracing::info!(version, "reloaded on SIGHUP");
                }
            }
        }
    }

    service.drain();
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
    let report = Stopped {
        proxy: proxy_server.await.unwrap_or_default(),
        admin: admin_server.await.unwrap_or_default(),
    };
    for (name, forced) in [
        ("proxy", report.proxy.forced),
        ("admin", report.admin.forced),
    ] {
        if forced > 0 {
            tracing::warn!(
                listener = name,
                forced,
                "shutdown timeout reached; closing remaining connections"
            );
        }
    }
    drop(stop_background);
    let _ = background.await;
    tracing::info!(
        accepted = report.proxy.accepted,
        forced = report.proxy.forced,
        "stopped"
    );
    Ok(report)
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
        let s = Settings::load("STEER", env(&[])).unwrap();
        assert_eq!(
            s,
            Settings {
                admin_token: String::new(),
                drain_period: Duration::from_secs(5),
                shutdown_timeout: Duration::from_secs(30),
            }
        );
        let s = Settings::load(
            "STEER",
            env(&[
                ("STEER_ADMIN_TOKEN", "0123456789abcdef"),
                ("STEER_DRAIN_PERIOD", "0s"),
                ("STEER_SHUTDOWN_TIMEOUT", "1h"),
            ]),
        )
        .unwrap();
        assert_eq!(s.admin_token, "0123456789abcdef");
        assert_eq!(s.drain_period, Duration::ZERO);
        assert_eq!(s.shutdown_timeout, Duration::from_secs(3600));
        let s = Settings::load("STEER", env(&[("STEER_DRAIN_PERIOD", "")])).unwrap();
        assert_eq!(s.drain_period, Duration::from_secs(5));
    }

    #[test]
    fn settings_refuse_what_steer_refused() {
        let err = |pairs: &[(&str, &str)]| Settings::load("EDGEFRONT", env(pairs)).unwrap_err();
        assert_eq!(
            err(&[("EDGEFRONT_ADMIN_TOKEN", "short")]),
            "EDGEFRONT_ADMIN_TOKEN must be at least 16 characters"
        );
        for bad in ["-1s", "11m", "soon", "5", "forever"] {
            assert_eq!(
                err(&[("EDGEFRONT_DRAIN_PERIOD", bad)]),
                "EDGEFRONT_DRAIN_PERIOD must be a duration between 0s and 10m0s",
                "{bad}"
            );
        }
        assert_eq!(
            err(&[("EDGEFRONT_SHUTDOWN_TIMEOUT", "-1s")]),
            "EDGEFRONT_SHUTDOWN_TIMEOUT must be a duration between 0s and 1h0m0s"
        );
        assert_eq!(
            err(&[("EDGEFRONT_SHUTDOWN_TIMEOUT", "1h0m1s")]),
            "EDGEFRONT_SHUTDOWN_TIMEOUT must be a duration between 0s and 1h0m0s"
        );
    }

    /// steer's TestLoopback, plus a few more.
    #[test]
    fn loopback_matches_steer() {
        for addr in ["127.0.0.1:9901", "[::1]:9901", "localhost:1", "127.9.9.9:0"] {
            assert!(loopback(addr), "{addr}");
        }
        for addr in [
            ":9901",
            "0.0.0.0:1",
            "[::]:1",
            "10.0.0.1:1",
            "example:1",
            "garbage",
        ] {
            assert!(!loopback(addr), "{addr}");
        }
    }

    #[test]
    fn listen_errors_name_the_address() {
        let e = ListenError {
            listen: "127.0.0.1:1".into(),
            error: std::io::Error::other("busy"),
        };
        assert_eq!(e.to_string(), "cannot listen on 127.0.0.1:1: busy");
        let l = admin_limits(Duration::from_secs(3));
        assert_eq!(l.max_header_bytes, 16 << 10);
        assert_eq!(l.drain_timeout, Duration::from_secs(3));
    }
}
