//! The daemon: listeners, accepting with descriptor exhaustion handled,
//! signals, reload and drain. Ported from pacerd's `daemon.c` and
//! `worker.c`.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use edgefront_limiter::cidr::Swap;
use edgefront_limiter::config::ListenerConfig;
use edgefront_limiter::policy::AddressPolicy;
use edgefront_limiter::{Admission, Bandwidth, LimiterConfig, LogLevel, Refusal};
use tokio::net::TcpListener;
use tokio::sync::{Notify, Semaphore, watch};
use tokio::time::Instant;

use edgefront_core::l4::{ACCEPT_PAUSE, Accepted, Acceptor};

use super::address::{self, Plan};
use super::conn::{self, Context};
use super::log::{Limit, Log};

/// The daemon's monotonic clock in nanoseconds since it started, the time
/// base of every limiter decision, and the [`Instant`] of any such time.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    start: Instant,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    pub fn now_ns(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    pub fn instant(&self, ns: u64) -> Instant {
        self.start + Duration::from_nanos(ns)
    }
}

/// One listener's shared state: its admission limits, byte limits,
/// refusal-log limit and live config.
pub struct Listener {
    pub admission: Arc<Admission>,
    pub bandwidth: Arc<Bandwidth>,
    /// pacerd's refusal log limit: 10 lines a second, bursts of 20.
    pub reject_log: Limit,
    live: RwLock<Arc<ListenerConfig>>,
}

impl Listener {
    fn new(l: &ListenerConfig) -> Result<Self, String> {
        let err = |e| format!("listener {}: cannot create client table: {e}", l.name);
        Ok(Self {
            admission: Arc::new(Admission::new(l).map_err(err)?),
            bandwidth: Arc::new(Bandwidth::new(l).map_err(err)?),
            reject_log: Limit::new(10, 20),
            live: RwLock::new(Arc::new(l.clone())),
        })
    }

    /// The listener's config as it is now.
    pub fn config(&self) -> Arc<ListenerConfig> {
        Arc::clone(&self.live.read().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn name(&self) -> &str {
        self.admission.name()
    }
}

/// Counts the connection tasks still running, and wakes a waiter when
/// none are.
#[derive(Default)]
struct Tasks {
    n: AtomicUsize,
    idle: Notify,
}

struct TaskGuard(Arc<Tasks>);

impl Tasks {
    fn start(self: &Arc<Self>) -> TaskGuard {
        self.n.fetch_add(1, Ordering::AcqRel);
        TaskGuard(Arc::clone(self))
    }

    async fn wait_idle(&self) {
        loop {
            let idle = self.idle.notified();
            if self.n.load(Ordering::Acquire) == 0 {
                return;
            }
            idle.await;
        }
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if self.0.n.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

/// The running daemon.
pub struct Daemon {
    path: PathBuf,
    pub clock: Clock,
    pub log: Arc<Log>,
    config: Mutex<Arc<LimiterConfig>>,
    /// The `[address_policy]` section, replaced whole on reload.
    policy: Arc<Swap<Option<Arc<AddressPolicy>>>>,
    /// Connections that may still wait for a request head.
    heads: Arc<Semaphore>,
    pub listeners: Vec<Arc<Listener>>,
    draining: AtomicBool,
    /// Wakes the main loop to drain, with the reason.
    drain_reason: Mutex<Option<&'static str>>,
    drain_now: Notify,
    tasks: Arc<Tasks>,
    /// True once the drain timeout has run out: open connections close.
    stop: Arc<super::stop::Stop>,
    /// True once draining: the accept loops stop and drop their sockets.
    accepting_tx: watch::Sender<bool>,
    /// True once drained: the worker threads stop.
    done_tx: watch::Sender<bool>,
}

/// Reads the config at `path`: a `.toml` file as the TOML limiter section,
/// anything else as a pacerd.conf. A parse error reads like pacerd's
/// (`line 2: listener life: missing }`).
pub fn load(path: &std::path::Path) -> Result<LimiterConfig, String> {
    if path.extension().is_some_and(|e| e == "toml") {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| e.to_string().trim_end().to_string())
    } else {
        edgefront_limiter::config::read_pacerd(path)
    }
}

/// Why `next` cannot replace `cur` in place, as pacerd's
/// `reload_conflict` words it.
pub fn reload_conflict(cur: &LimiterConfig, next: &LimiterConfig) -> Option<String> {
    if next.workers != cur.workers {
        return Some("workers cannot change without a restart".into());
    }
    if next.admin_socket != cur.admin_socket {
        return Some("admin_socket cannot change without a restart".into());
    }
    if next.listeners.len() != cur.listeners.len() {
        return Some("listeners cannot be added or removed without a restart".into());
    }
    for (a, b) in cur.listeners.iter().zip(&next.listeners) {
        let what = if a.name != b.name {
            "name"
        } else if a.listen != b.listen {
            "listen"
        } else if a.table_size != b.table_size {
            "table_size"
        } else if a.table_full != b.table_full {
            "table_full"
        } else {
            continue;
        };
        return Some(format!(
            "listener {}: {what} cannot change without a restart",
            a.name
        ));
    }
    None
}

impl Daemon {
    /// The daemon for `config` and its address `policy`, read from
    /// `path`, with every listener's tables allocated.
    pub fn new(
        path: PathBuf,
        config: LimiterConfig,
        policy: Option<AddressPolicy>,
        log: Arc<Log>,
    ) -> Result<Self, String> {
        let listeners = config
            .listeners
            .iter()
            .map(|l| Listener::new(l).map(Arc::new))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            path,
            clock: Clock::new(),
            log,
            config: Mutex::new(Arc::new(config)),
            policy: Arc::new(Swap::new(policy.map(Arc::new))),
            heads: Arc::new(Semaphore::new(address::HEAD_WAITS)),
            listeners,
            draining: AtomicBool::new(false),
            drain_reason: Mutex::new(None),
            drain_now: Notify::new(),
            tasks: Arc::default(),
            stop: Arc::default(),
            accepting_tx: watch::channel(true).0,
            done_tx: watch::channel(false).0,
        })
    }

    /// The config as it is now.
    pub fn config(&self) -> Arc<LimiterConfig> {
        Arc::clone(&self.config.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// Connections open across every listener.
    pub fn open(&self) -> usize {
        self.tasks.n.load(Ordering::Acquire)
    }

    /// Starts the drain, once; later calls do nothing.
    pub fn drain(&self, why: &'static str) {
        let mut r = self.drain_reason.lock().unwrap_or_else(|e| e.into_inner());
        if r.is_none() {
            *r = Some(why);
            self.drain_now.notify_one();
        }
    }

    /// Re-reads the config file and applies it in place, or refuses it and
    /// keeps the running config. Logs the outcome as pacerd does.
    pub fn reload(&self, source: &str) -> Result<(), String> {
        if self.is_draining() {
            self.log
                .msg(LogLevel::Warn, "reload ignored while draining");
            return Err("draining".into());
        }
        let result = self.try_reload();
        match &result {
            Ok(()) => drop(
                self.log
                    .line(LogLevel::Info, "reloaded")
                    .str("source", source),
            ),
            Err(e) => drop(
                self.log
                    .line(
                        LogLevel::Error,
                        "reload rejected, keeping the running config",
                    )
                    .str("source", source)
                    .str("error", e),
            ),
        }
        result
    }

    /// The address policy as it is now.
    pub fn policy(&self) -> Option<Arc<AddressPolicy>> {
        self.policy.reader().get().clone()
    }

    fn try_reload(&self) -> Result<(), String> {
        let next = load(&self.path)?;
        // Built in full, list files and all, before anything changes; the
        // running policy answers until the store below.
        let policy = address::load(&self.path)?;
        let mut cur = self.config.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = reload_conflict(&cur, &next) {
            return Err(c);
        }
        // Check every listener before changing any, so a refusal changes
        // nothing; the rates were validated by the reader, so only a
        // quota switched on where there was none can still be refused.
        for (l, nl) in self.listeners.iter().zip(&next.listeners) {
            if nl.per_ip_conn_quota.is_some() && l.admission.quota_stats().is_none() {
                return Err(format!(
                    "listener {}: per_ip_conn_quota cannot be switched on without a restart",
                    nl.name
                ));
            }
        }
        for (l, nl) in self.listeners.iter().zip(&next.listeners) {
            l.admission.reconfigure(nl)?;
            l.bandwidth.reconfigure(nl)?;
            *l.live.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(nl.clone());
        }
        self.log.set_level(next.log_level);
        *cur = Arc::new(next);
        self.policy.store(policy.map(|(p, _)| Arc::new(p)));
        Ok(())
    }

    fn context(&self) -> Context {
        Context {
            clock: self.clock,
            log: Arc::clone(&self.log),
            stop: Arc::clone(&self.stop),
        }
    }

    /// Accepts on `socket` for `listener` until the drain starts, then
    /// drops the socket so new connections are refused at once. Running
    /// out of descriptors sheds or pauses as [`Acceptor`] does, counted
    /// and logged as pacerd's `fd_exhausted` and `fd_exhausted_paused`.
    pub async fn accept_loop(self: Arc<Self>, listener: Arc<Listener>, socket: TcpListener) {
        let mut accepting = self.accepting_tx.subscribe();
        let mut acceptor = Acceptor::new(socket);
        let mut policy = self.policy.reader();
        loop {
            let accepted = tokio::select! {
                _ = accepting.wait_for(|on| !*on) => return,
                r = acceptor.accept() => r,
            };
            let refusal = match accepted {
                Ok(Accepted::Conn(stream, peer)) => {
                    let p = policy.get().clone();
                    self.start(&listener, stream, peer, p);
                    continue;
                }
                Ok(Accepted::Shed) => Refusal::FdExhausted,
                Ok(Accepted::Paused) => Refusal::FdExhaustedPaused,
                Err(_) => {
                    // Not a descriptor shortage and not a vanished peer:
                    // pause so a failing accept cannot spin.
                    tokio::select! {
                        _ = accepting.wait_for(|on| !*on) => return,
                        () = tokio::time::sleep(ACCEPT_PAUSE) => continue,
                    }
                }
            };
            listener.admission.refuse(refusal);
            let ctx = self.context();
            conn::log_refusal(&ctx, &listener, "-", refusal, self.clock.now_ns());
        }
    }

    fn start(
        &self,
        listener: &Arc<Listener>,
        stream: tokio::net::TcpStream,
        peer: SocketAddr,
        policy: Option<Arc<AddressPolicy>>,
    ) {
        let ctx = self.context();
        let now = self.clock.now_ns();
        let Some(policy) = policy else {
            let Ok(permit) = conn::admit(&ctx, listener, peer.ip(), peer, now) else {
                // Dropping the stream closes it with nothing read.
                return;
            };
            let guard = self.tasks.start();
            let listener = Arc::clone(listener);
            tokio::spawn(async move {
                conn::serve(ctx, listener, stream, peer, permit, None, now).await;
                drop(guard);
            });
            return;
        };
        let Ok(waiting) = Arc::clone(&self.heads).try_acquire_owned() else {
            listener.admission.refuse(Refusal::ListenerFull);
            conn::log_refusal(
                &ctx,
                listener,
                &peer.to_string(),
                Refusal::ListenerFull,
                now,
            );
            return;
        };
        let guard = self.tasks.start();
        let listener = Arc::clone(listener);
        let clock = self.clock;
        tokio::spawn(async move {
            let wait = listener.config().connect_timeout;
            let plan = address::decide(
                &policy,
                &stream,
                peer.ip(),
                wait,
                || clock.now_ns(),
                &ctx.log,
            )
            .await;
            drop(waiting);
            let now = clock.now_ns();
            let (permit, class) = match plan {
                // The peer closed before sending a head.
                None => return,
                Some(Plan::Listener(client)) => {
                    match conn::admit(&ctx, &listener, client, peer, now) {
                        Ok(p) => (p, None),
                        Err(_) => return,
                    }
                }
                Some(Plan::Slot(client, class)) => match listener.admission.admit_slot(client) {
                    Ok(p) => (p, class),
                    Err(r) => {
                        conn::log_refusal(&ctx, &listener, &peer.to_string(), r, now);
                        return;
                    }
                },
                Some(Plan::Refuse(reason, class, client)) => {
                    conn::log_class_refusal(
                        &ctx,
                        &listener,
                        &peer.to_string(),
                        reason,
                        &class,
                        &client.to_string(),
                        now,
                    );
                    return;
                }
            };
            conn::serve(ctx, listener, stream, peer, permit, class, now).await;
            drop(guard);
        });
    }

    /// Lets the worker threads stop: the drain is over, or the daemon
    /// failed to start.
    pub fn finish(&self) {
        self.accepting_tx.send_replace(false);
        self.stop.raise();
        self.done_tx.send_replace(true);
    }

    /// Waits for the drain, then lets open connections finish until the
    /// drain timeout and closes what is left.
    pub async fn wait_and_drain(&self) {
        loop {
            let notified = self.drain_now.notified();
            let why = *self.drain_reason.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(why) = why {
                self.run_drain(why).await;
                return;
            }
            notified.await;
        }
    }

    async fn run_drain(&self, why: &str) {
        let timeout = self.config().drain_timeout;
        drop(
            self.log
                .line(LogLevel::Info, "draining")
                .str("reason", why)
                .u64(
                    "timeout_ms",
                    u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                ),
        );
        self.draining.store(true, Ordering::Release);
        self.accepting_tx.send_replace(false);
        if tokio::time::timeout(timeout, self.tasks.wait_idle())
            .await
            .is_err()
        {
            drop(
                self.log
                    .line(LogLevel::Warn, "drain timeout")
                    .u64("worker", 0)
                    .u64("closing", self.open() as u64),
            );
            self.stop.raise();
            self.tasks.wait_idle().await;
        }
    }
}

/// Binds `workers` sockets for every listener, `[worker][listener]`, or
/// names the first address that failed, as pacerd's `open_listener` did:
/// the address is first claimed once without `SO_REUSEPORT`, so a port
/// someone else holds fails here, and then each worker gets a socket of
/// its own with `SO_REUSEPORT`, among which the kernel spreads new
/// connections (Linux balances them; macOS gives them to one socket).
pub fn bind(
    cfg: &LimiterConfig,
    workers: u32,
) -> Result<Vec<Vec<TcpListener>>, (SocketAddr, io::Error)> {
    use tokio::net::TcpSocket;
    let socket = |addr: SocketAddr| {
        if addr.is_ipv4() {
            TcpSocket::new_v4()
        } else {
            TcpSocket::new_v6()
        }
    };
    for l in &cfg.listeners {
        let probe = socket(l.listen).map_err(|e| (l.listen, e))?;
        // A restart binds at once while old connections sit in TIME_WAIT.
        probe.set_reuseaddr(true).map_err(|e| (l.listen, e))?;
        probe.bind(l.listen).map_err(|e| (l.listen, e))?;
    }
    (0..workers.max(1))
        .map(|_| {
            cfg.listeners
                .iter()
                .map(|l| {
                    let err = |e| (l.listen, e);
                    let s = socket(l.listen).map_err(err)?;
                    s.set_reuseaddr(true).map_err(err)?;
                    s.set_reuseport(true).map_err(err)?;
                    s.bind(l.listen).map_err(err)?;
                    s.listen(1024).map_err(err)
                })
                .collect()
        })
        .collect()
}

/// Runs the daemon from `path` until it has drained. Exit codes as
/// pacerd's: 0 after a drain, 1 when it cannot start, 2 for a bad config.
pub fn run(path: PathBuf) -> ExitCode {
    let log = Arc::new(Log::new(LogLevel::Info));
    let loaded = load(&path).and_then(|c| Ok((c, address::load(&path)?)));
    let (config, policy) = match loaded {
        Ok((c, p)) => (c, p.map(|(p, _)| p)),
        Err(e) => {
            drop(
                log.line(LogLevel::Error, "bad config")
                    .str("error", &format!("{}: {e}", path.display())),
            );
            return ExitCode::from(2);
        }
    };
    log.set_level(config.log_level);
    // The main thread runs signals, the admin socket and the drain; the
    // connections run on `workers` threads of their own (see `serve`).
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            drop(
                log.line(LogLevel::Error, "cannot start the runtime")
                    .str("error", &e.to_string()),
            );
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(serve(path, config, policy, log))
}

async fn serve(
    path: PathBuf,
    config: LimiterConfig,
    policy: Option<AddressPolicy>,
    log: Arc<Log>,
) -> ExitCode {
    let sockets = match bind(&config, config.workers) {
        Ok(s) => s,
        Err((addr, e)) => {
            drop(
                log.line(LogLevel::Error, "cannot listen")
                    .str("what", &addr.to_string())
                    .str("error", &e.to_string()),
            );
            return ExitCode::FAILURE;
        }
    };
    let admin_path = config.admin_socket.clone();
    let daemon = match Daemon::new(path, config, policy, Arc::clone(&log)) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            drop(log.line(LogLevel::Error, &e));
            return ExitCode::FAILURE;
        }
    };
    let admin = match &admin_path {
        Some(p) => match super::admin::open(p) {
            Ok(a) => Some(a),
            Err(e) => {
                drop(
                    log.line(LogLevel::Error, e.msg)
                        .str("path", &p.display().to_string())
                        .str("error", &e.error),
                );
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };
    let signals = match Signals::new() {
        Ok(s) => s,
        Err(e) => {
            drop(
                log.line(LogLevel::Error, "cannot create signal or event fd")
                    .str("error", &e.to_string()),
            );
            return ExitCode::FAILURE;
        }
    };
    let workers = match start_workers(&daemon, sockets) {
        Ok(w) => w,
        Err(e) => {
            drop(
                log.line(LogLevel::Error, "cannot start worker")
                    .str("error", &e.to_string()),
            );
            daemon.finish();
            return ExitCode::FAILURE;
        }
    };
    let admin_task = admin.map(|a| tokio::spawn(super::admin::serve(a, Arc::clone(&daemon))));
    let cfg = daemon.config();
    drop(
        log.line(LogLevel::Info, "ready")
            .str("version", env!("CARGO_PKG_VERSION"))
            .u64("workers", u64::from(cfg.workers))
            .u64("listeners", cfg.listeners.len() as u64),
    );
    let sig = tokio::spawn(signals.run(Arc::clone(&daemon)));
    daemon.wait_and_drain().await;
    daemon.finish();
    for w in workers {
        let _ = w.join();
    }
    sig.abort();
    if let Some(t) = admin_task {
        t.abort();
        let _ = t.await;
    }
    if let Some(p) = &admin_path {
        let _ = std::fs::remove_file(p);
    }
    log.msg(LogLevel::Info, "drained");
    ExitCode::SUCCESS
}

/// Starts one thread per worker, as pacerd did, each with a
/// single-threaded runtime that accepts on its own sockets and runs the
/// connections it accepts to their end: a connection never moves between
/// threads, and no wake crosses one. Each thread stops once the daemon
/// has drained ([`Daemon::finish`]).
fn start_workers(
    daemon: &Arc<Daemon>,
    sockets: Vec<Vec<TcpListener>>,
) -> io::Result<Vec<std::thread::JoinHandle<()>>> {
    let mut threads = Vec::with_capacity(sockets.len());
    for (i, mine) in sockets.into_iter().enumerate() {
        let copies = mine
            .into_iter()
            .map(TcpListener::into_std)
            .collect::<io::Result<Vec<_>>>()?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let d = Arc::clone(daemon);
        let mut done = d.done_tx.subscribe();
        threads.push(
            std::thread::Builder::new()
                .name(format!("worker-{i}"))
                .spawn(move || {
                    rt.block_on(async move {
                        for (l, s) in d.listeners.iter().zip(copies) {
                            // into_std leaves the socket non-blocking.
                            match TcpListener::from_std(s) {
                                Ok(s) => {
                                    tokio::spawn(Arc::clone(&d).accept_loop(Arc::clone(l), s));
                                }
                                Err(e) => {
                                    drop(
                                        d.log
                                            .line(LogLevel::Error, "cannot listen")
                                            .str("what", l.name())
                                            .str("error", &e.to_string()),
                                    );
                                    d.drain("worker failure");
                                }
                            }
                        }
                        let _ = done.wait_for(|v| *v).await;
                    });
                })?,
        );
    }
    // When the drain stops the accept loops, each drops its socket, and
    // once all are closed new connections are refused.
    Ok(threads)
}

/// SIGTERM and SIGINT drain; SIGHUP reloads.
struct Signals {
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
}

impl Signals {
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
            hup: signal(SignalKind::hangup())?,
        })
    }

    async fn run(mut self, d: Arc<Daemon>) {
        loop {
            tokio::select! {
                _ = self.term.recv() => d.drain("SIGTERM"),
                _ = self.int.recv() => d.drain("SIGINT"),
                _ = self.hup.recv() => {
                    let _ = d.reload("SIGHUP");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> LimiterConfig {
        edgefront_limiter::parse_pacerd(text.as_bytes()).unwrap()
    }

    const BASE: &str =
        "workers 2\nlistener a {\n listen 127.0.0.1:1000\n upstream 127.0.0.1:2000\n}\n";

    #[test]
    fn reload_conflicts_use_pacerds_words() {
        let cur = cfg(BASE);
        assert_eq!(reload_conflict(&cur, &cur), None);
        let mut moved = cur.clone();
        moved.listeners[0].upstream = "127.0.0.1:3000".parse().unwrap();
        moved.listeners[0].per_ip_max_conns = 3;
        assert_eq!(reload_conflict(&cur, &moved), None);
        type Edit = fn(&mut LimiterConfig);
        let cases: [(Edit, &str); 7] = [
            (|c| c.workers = 3, "workers cannot change without a restart"),
            (
                |c| c.admin_socket = Some("/tmp/x".into()),
                "admin_socket cannot change without a restart",
            ),
            (
                |c| {
                    let l = c.listeners[0].clone();
                    c.listeners.push(l);
                },
                "listeners cannot be added or removed without a restart",
            ),
            (
                |c| c.listeners[0].name = "b".into(),
                "listener a: name cannot change without a restart",
            ),
            (
                |c| c.listeners[0].listen = "127.0.0.1:1001".parse().unwrap(),
                "listener a: listen cannot change without a restart",
            ),
            (
                |c| c.listeners[0].table_size = 1024,
                "listener a: table_size cannot change without a restart",
            ),
            (
                |c| c.listeners[0].table_full = edgefront_limiter::FullPolicy::Evict,
                "listener a: table_full cannot change without a restart",
            ),
        ];
        for (edit, want) in cases {
            let mut next = cur.clone();
            edit(&mut next);
            assert_eq!(reload_conflict(&cur, &next).as_deref(), Some(want));
        }
    }

    #[test]
    fn the_clock_maps_both_ways() {
        let c = Clock::new();
        let a = c.now_ns();
        let b = c.now_ns();
        assert!(b >= a);
        assert_eq!(c.instant(5) - c.instant(2), Duration::from_nanos(3));
    }

    #[tokio::test]
    async fn the_task_count_wakes_its_waiter_at_zero() {
        let t = Arc::new(Tasks::default());
        t.wait_idle().await;
        let g1 = t.start();
        let g2 = t.start();
        let waiter = {
            let t = Arc::clone(&t);
            tokio::spawn(async move { t.wait_idle().await })
        };
        drop(g1);
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(g2);
        waiter.await.unwrap();
    }
}
