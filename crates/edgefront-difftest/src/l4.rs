//! Traffic scripts for pacerd's data path: the scenarios of pacer's
//! `tests/it_*.c`, written as TOML files under `tests/pacer_scripts/` and
//! run against a running binary, either the legacy `pacerd` (with
//! `pacerctl`) or `edgefront limits` (with `edgefront ctl limits`).
//!
//! A script names its upstreams, the config of its one listener, and a
//! list of steps. Each step connects, sends, waits, signals, rewrites the
//! config, reads the log or the counters, and so on, and gives one outcome
//! line: `works a: yes`, `log "reason=per_ip_conns": found`,
//! `stats: accepted=5 rejected_per_ip_rate=1`. A step may say what its
//! outcome must be (`expect`), and every run's outcomes form a transcript.
//! [`diff`] compares two binaries' transcripts line by line, so the same
//! script proves that pacerd and edgefront accept, refuse and close the
//! same connections and count them the same way. Timing bounds (a shaped
//! download takes 1.8 to 2.4 s) are part of an outcome as `time=ok`, so a
//! binary that is out of bounds both fails its expectation and diverges.
//!
//! Config text has placeholders: `{port}` (the listener's port, picked
//! free per run), `{upstream}` or `{upstream:NAME}` (an upstream's
//! address) and `{sock}` (the admin socket). Log texts take `{peer:CONN}`,
//! the local address of connection `CONN`, which is the peer pacerd logs.
//!
//! Steps run on blocking sockets, one thread per background job; the
//! upstreams are threads too. Nothing here is specific to either binary
//! but the command lines in [`Target`].

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Deserialize;

/// A binary to run scripts against.
#[derive(Clone, Debug)]
pub enum Target {
    /// The legacy daemon: `pacerd -c CONFIG` and `pacerctl -s SOCK CMD`.
    Pacerd { pacerd: PathBuf, pacerctl: PathBuf },
    /// `edgefront limits -c CONFIG` and `edgefront ctl limits -s SOCK CMD`.
    Edgefront { bin: PathBuf },
}

impl Target {
    /// `pacerd` or `edgefront`, as scripts name a target.
    pub fn name(&self) -> &'static str {
        match self {
            Target::Pacerd { .. } => "pacerd",
            Target::Edgefront { .. } => "edgefront",
        }
    }

    fn daemon(&self, conf: &Path) -> (PathBuf, Vec<String>) {
        let c = conf.display().to_string();
        match self {
            Target::Pacerd { pacerd, .. } => (pacerd.clone(), vec!["-c".into(), c]),
            Target::Edgefront { bin } => (bin.clone(), vec!["limits".into(), "-c".into(), c]),
        }
    }

    fn ctl(&self, sock: &Path, cmd: &str) -> Command {
        let s = sock.display().to_string();
        match self {
            Target::Pacerd { pacerctl, .. } => {
                let mut c = Command::new(pacerctl);
                c.args(["-s", &s, cmd]);
                c
            }
            Target::Edgefront { bin } => {
                let mut c = Command::new(bin);
                c.args(["ctl", "limits", "-s", &s, cmd]);
                c
            }
        }
    }
}

/// A script, as read from its TOML file.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Script {
    /// The scenario's name, the `TEST(...)` it comes from.
    pub name: String,
    /// The C test it is ported from.
    pub from: String,
    /// Set when the script needs loopback addresses other than
    /// 127.0.0.1, which only Linux has without configuration.
    #[serde(default)]
    pub needs_loopback_range: bool,
    /// A descriptor limit for the daemon, per target name.
    #[serde(default)]
    pub nofile: BTreeMap<String, u32>,
    /// The upstreams; the first is the listener's unless the config says
    /// otherwise.
    pub upstream: Vec<UpstreamSpec>,
    /// The config the daemon starts with.
    pub config: String,
    /// The steps, in order.
    pub step: Vec<Step>,
}

/// One upstream a script runs.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamSpec {
    pub name: String,
    pub mode: Mode,
    /// For `source`: how many pattern bytes to send.
    #[serde(default)]
    pub bytes: u64,
}

/// What an upstream does with each connection: the modes of pacer's
/// `tests/upstream.c`.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Echo until end of stream, then close.
    Echo,
    /// Read to end of stream, reply `<fnv64 hex> <count>\n`, close.
    Hash,
    /// Send `bytes` pattern bytes, then close.
    Source,
    /// Accept, then neither read nor write.
    Stall,
}

/// One step. `op` picks what it does; the other fields are its arguments.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub op: String,
    /// The connection the step works on.
    #[serde(default)]
    pub conn: Option<String>,
    /// The loopback address to connect from.
    #[serde(default)]
    pub from: Option<String>,
    /// Text: log text to find, a config to write, a ctl command, a signal.
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub bytes: Option<u64>,
    #[serde(default)]
    pub count: Option<u64>,
    #[serde(default)]
    pub ms: Option<u64>,
    #[serde(default)]
    pub min_ms: Option<u64>,
    #[serde(default)]
    pub max_ms: Option<u64>,
    /// For `download`: at most this many times as long as the same
    /// download straight from the upstream, timed in the same run just
    /// before, instead of a wall-clock bound that a slow or busy machine
    /// breaks.
    #[serde(default)]
    pub max_vs_direct: Option<u64>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Counter names for `stats`.
    #[serde(default)]
    pub counters: Vec<String>,
    /// Addresses for `churn` and `download_together`.
    #[serde(default)]
    pub sources: Vec<String>,
    /// What the outcome must be.
    #[serde(default)]
    pub expect: Option<String>,
}

/// Reads every `*.toml` script in `dir`, sorted by file name.
pub fn load_dir(dir: &Path) -> Result<Vec<(String, Script)>, String> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            let s: Script = toml::from_str(&text)
                .map_err(|e| format!("{}: {}", p.display(), e.to_string().trim_end()))?;
            let file = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            Ok((file, s))
        })
        .collect()
}

/// One run's outcomes, one line per step.
pub type Transcript = Vec<String>;

/// The outcome of running a script on one target: the transcript, and the
/// steps whose outcome differed from their `expect`.
#[derive(Debug, Default)]
pub struct Run {
    pub transcript: Transcript,
    pub failed: Vec<String>,
    /// The daemon's log, for reports.
    pub log: String,
}

/// The first line where two transcripts differ, as a report.
pub fn diff(a_name: &str, a: &[String], b_name: &str, b: &[String]) -> Option<String> {
    let n = a.len().max(b.len());
    for i in 0..n {
        let (x, y) = (a.get(i), b.get(i));
        if x != y {
            return Some(format!(
                "step {}: {a_name}: {}\n         {b_name}: {}",
                i + 1,
                x.map_or("(none)", String::as_str),
                y.map_or("(none)", String::as_str)
            ));
        }
    }
    None
}

/// Whether this host can connect from loopback addresses other than
/// 127.0.0.1.
pub fn has_loopback_range() -> bool {
    let Ok(l) = TcpListener::bind("127.0.0.1:0") else {
        return false;
    };
    let Ok(a) = l.local_addr() else {
        return false;
    };
    connect_from(Some("127.0.0.2"), a, Duration::from_secs(1)).is_ok()
}

/// A free TCP port on 127.0.0.1, a moment ago.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(0)
}

/// pacer's test pattern.
fn pattern(buf: &mut [u8], off: u64) {
    for (i, b) in buf.iter_mut().enumerate() {
        let x = off + i as u64;
        *b = ((x.wrapping_mul(2_654_435_761) >> 13) ^ x) as u8;
    }
}

const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;

fn fnv(mut h: u64, data: &[u8]) -> u64 {
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// A running upstream.
struct Upstream {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Upstream {
    fn start(spec: &UpstreamSpec) -> io::Result<Self> {
        let l = TcpListener::bind("127.0.0.1:0")?;
        let addr = l.local_addr()?;
        l.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let (mode, bytes, s2) = (spec.mode, spec.bytes, Arc::clone(&stop));
        let thread = std::thread::spawn(move || {
            let mut held = Vec::new();
            while !s2.load(Ordering::Relaxed) {
                match l.accept() {
                    Ok((s, _)) => {
                        let _ = s.set_nonblocking(false);
                        if mode == Mode::Stall {
                            held.push(s);
                        } else {
                            std::thread::spawn(move || serve(s, mode, bytes));
                        }
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Ok(Self {
            addr,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve(mut s: TcpStream, mode: Mode, bytes: u64) {
    let mut buf = vec![0u8; 64 * 1024];
    match mode {
        Mode::Echo => loop {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if s.write_all(&buf[..n]).is_err() {
                        return;
                    }
                }
            }
        },
        Mode::Hash => {
            let (mut h, mut count) = (FNV_START, 0u64);
            loop {
                match s.read(&mut buf) {
                    Ok(0) => break,
                    Err(_) => return,
                    Ok(n) => {
                        h = fnv(h, &buf[..n]);
                        count += n as u64;
                    }
                }
            }
            let _ = s.write_all(format!("{h:016x} {count}\n").as_bytes());
        }
        Mode::Source => {
            let mut off = 0;
            while off < bytes {
                let n = (bytes - off).min(buf.len() as u64) as usize;
                pattern(&mut buf[..n], off);
                if s.write_all(&buf[..n]).is_err() {
                    return;
                }
                off += n as u64;
            }
        }
        Mode::Stall => {}
    }
}

fn connect_from(from: Option<&str>, to: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    match from {
        None | Some("127.0.0.1") => TcpStream::connect_timeout(&to, timeout),
        Some(src) => {
            // std cannot bind before connect; go through the shell-free
            // route of a bound listener's address family: tokio's socket.
            let src: std::net::IpAddr = src
                .parse()
                .map_err(|_| io::Error::other(format!("bad address {src}")))?;
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(async {
                let s = tokio::net::TcpSocket::new_v4()?;
                s.bind(SocketAddr::new(src, 0))?;
                let s = tokio::time::timeout(timeout, s.connect(to))
                    .await
                    .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
                s.into_std()
            })
            .and_then(|s| {
                s.set_nonblocking(false)?;
                Ok(s)
            })
        }
    }
}

/// The daemon under test.
struct Daemon {
    child: Option<Child>,
    log: PathBuf,
}

impl Daemon {
    fn text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(c) = &mut self.child {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Everything one run of a script holds.
struct Ctx<'a> {
    target: &'a Target,
    script: &'a Script,
    dir: PathBuf,
    conf: PathBuf,
    sock: PathBuf,
    port: u16,
    ups: BTreeMap<String, Upstream>,
    daemon: Daemon,
    conns: BTreeMap<String, TcpStream>,
    /// Each connection's local address, kept after it closes.
    peers: BTreeMap<String, String>,
    jobs: BTreeMap<String, JoinHandle<String>>,
    marks: BTreeMap<String, Instant>,
}

impl Ctx<'_> {
    fn render(&self, text: &str) -> String {
        let mut out = text
            .replace("{port}", &self.port.to_string())
            .replace("{sock}", &self.sock.display().to_string());
        if let Some(first) = self.script.upstream.first() {
            out = out.replace("{upstream}", &self.ups[&first.name].addr.to_string());
        }
        for (name, u) in &self.ups {
            out = out.replace(&format!("{{upstream:{name}}}"), &u.addr.to_string());
        }
        for (name, a) in &self.peers {
            out = out.replace(&format!("{{peer:{name}}}"), a);
        }
        out
    }

    fn listener(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }

    /// Starts the daemon on the config file and waits for `ready`.
    fn start(&mut self) -> Result<(), String> {
        let (bin, args) = self.target.daemon(&self.conf);
        let err = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.daemon.log)
            .map_err(|e| e.to_string())?;
        let limit = self
            .script
            .nofile
            .get(self.target.name())
            .map_or(String::new(), |n| format!("ulimit -n {n} && "));
        let child = Command::new("sh")
            .arg("-c")
            .arg(format!("{limit}exec \"$0\" \"$@\""))
            .arg(bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(err)
            .spawn()
            .map_err(|e| e.to_string())?;
        let before = self.daemon.text().matches("msg=ready").count();
        self.daemon.child = Some(child);
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if self.daemon.text().matches("msg=ready").count() > before {
                return Ok(());
            }
            if let Some(c) = &mut self.daemon.child
                && let Ok(Some(st)) = c.try_wait()
            {
                return Err(format!("exited with {st} before ready"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err("no ready line in 10 s".into())
    }

    fn pid(&self) -> Option<u32> {
        self.daemon.child.as_ref().map(Child::id)
    }

    fn signal(&self, sig: &str) -> bool {
        self.pid().is_some_and(|pid| {
            Command::new("kill")
                .arg(format!("-{sig}"))
                .arg(pid.to_string())
                .status()
                .is_ok_and(|s| s.success())
        })
    }

    fn wait_log(&self, needle: &str, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        loop {
            if self.daemon.text().contains(needle) {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn conn(&mut self, step: &Step) -> Result<&mut TcpStream, String> {
        let name = step.conn.as_deref().ok_or("step needs conn")?;
        self.conns
            .get_mut(name)
            .ok_or_else(|| format!("no connection {name}"))
    }

    fn cpu_secs(&self) -> f64 {
        let Some(pid) = self.pid() else { return 0.0 };
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            let rest = &stat[stat.rfind(')').map_or(0, |i| i + 2)..];
            let f: Vec<&str> = rest.split(' ').collect();
            let num = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
            return (num(11) + num(12)) / 100.0;
        }
        let out = Command::new("ps")
            .args(["-o", "time=", "-p", &pid.to_string()])
            .output();
        out.ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .map_or(0.0, |t| {
                t.split(':')
                    .fold(0.0, |acc, p| acc * 60.0 + p.parse::<f64>().unwrap_or(0.0))
            })
    }

    fn ctl(&self, cmd: &str) -> (i32, String) {
        match self.target.ctl(&self.sock, cmd).output() {
            Ok(o) => (
                o.status.code().unwrap_or(-1),
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                ),
            ),
            Err(e) => (-1, e.to_string()),
        }
    }
}

fn timeout(step: &Step, default_ms: u64) -> Duration {
    Duration::from_millis(step.timeout_ms.unwrap_or(default_ms))
}

/// `time=ok` when `took` is within the step's bounds, else the time.
fn timed(step: &Step, took: Duration) -> String {
    let ms = took.as_millis() as u64;
    let lo = step.min_ms.unwrap_or(0);
    let hi = step.max_ms.unwrap_or(u64::MAX);
    if (lo..=hi).contains(&ms) {
        "time=ok".into()
    } else {
        format!("time={ms}ms, outside {lo}..{hi}")
    }
}

/// Whether a connection proxies an echo of "ping".
fn works(s: &mut TcpStream) -> bool {
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    let mut b = [0u8; 4];
    s.write_all(b"ping").is_ok() && s.read_exact(&mut b).is_ok() && &b == b"ping"
}

/// Whether the proxy closed a connection with nothing sent.
fn refused(s: &mut TcpStream) -> bool {
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    let mut b = [0u8; 1];
    matches!(s.read(&mut b), Ok(0) | Err(_))
        && !matches!(s.read(&mut b), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
}

/// Reads to end of stream within `limit`: (bytes, hash, whether it ended).
fn read_to_eof(s: &mut TcpStream, limit: Duration) -> (u64, u64, bool) {
    let end = Instant::now() + limit;
    let mut buf = vec![0u8; 64 * 1024];
    let (mut n, mut h) = (0u64, FNV_START);
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return (n, h, false);
        }
        let _ = s.set_read_timeout(Some(left));
        match s.read(&mut buf) {
            Ok(0) => return (n, h, true),
            Ok(k) => {
                h = fnv(h, &buf[..k]);
                n += k as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return (n, h, true),
        }
    }
}

fn pattern_hash(total: u64) -> u64 {
    let mut buf = vec![0u8; 64 * 1024];
    let (mut off, mut h) = (0, FNV_START);
    while off < total {
        let n = (total - off).min(buf.len() as u64) as usize;
        pattern(&mut buf[..n], off);
        h = fnv(h, &buf[..n]);
        off += n as u64;
    }
    h
}

/// Parses `listener=NAME open=N key=value ...` lines into one map per
/// listener, with `open` as a counter.
pub fn parse_stats(text: &str) -> BTreeMap<String, BTreeMap<String, u64>> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let mut fields = line.split(' ');
        let Some(name) = fields.next().and_then(|f| f.strip_prefix("listener=")) else {
            continue;
        };
        let m: BTreeMap<String, u64> = fields
            .filter_map(|f| f.split_once('='))
            .filter_map(|(k, v)| v.parse().ok().map(|v| (k.to_string(), v)))
            .collect();
        out.insert(name.to_string(), m);
    }
    out
}

const CLOSES: [&str; 7] = [
    "closed_done",
    "closed_idle",
    "closed_slow",
    "closed_upstream_failed",
    "closed_reset",
    "closed_drain",
    "closed_other",
];

/// Runs one step and returns its outcome.
fn step(ctx: &mut Ctx<'_>, st: &Step) -> Result<String, String> {
    let to = ctx.listener();
    Ok(match st.op.as_str() {
        "connect" => {
            let name = st.conn.clone().ok_or("connect needs conn")?;
            match connect_from(st.from.as_deref(), to, Duration::from_secs(3)) {
                Ok(s) => {
                    if let Ok(a) = s.local_addr() {
                        ctx.peers.insert(name.clone(), a.to_string());
                    }
                    ctx.conns.insert(name, s);
                    "ok".into()
                }
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => "refused".into(),
                Err(e) => format!("error {:?}", e.kind()),
            }
        }
        "works" => yes(works(ctx.conn(st)?)),
        "refused" => yes(refused(ctx.conn(st)?)),
        "close" => {
            let name = st.conn.as_deref().ok_or("close needs conn")?;
            ctx.conns.remove(name).ok_or("no such connection")?;
            "ok".into()
        }
        "send" => {
            // `text`, or `bytes` bytes of 'y'.
            let data = match st.bytes {
                Some(n) => vec![b'y'; n as usize],
                None => st.text.clone().unwrap_or_default().into_bytes(),
            };
            yes(ctx.conn(st)?.write_all(&data).is_ok())
        }
        "works_n" => {
            // `count` connections in turn, each proxying an echo and closed.
            let n = st.count.ok_or("works_n needs count")?;
            let ok = (0..n)
                .filter(|_| {
                    TcpStream::connect_timeout(&to, Duration::from_secs(3))
                        .is_ok_and(|mut s| works(&mut s))
                })
                .count();
            format!("{ok} of {n}")
        }
        "cycle" => {
            // it_limits.c's slot test: `count` rounds of connect, echo,
            // close, and wait for the close to be logged before the next.
            let n = st.count.ok_or("cycle needs count")?;
            let mut out = format!("{n} rounds");
            for i in 0..n {
                let Ok(mut s) = TcpStream::connect_timeout(&to, Duration::from_secs(3)) else {
                    out = format!("connect failed in round {i}");
                    break;
                };
                if !works(&mut s) {
                    out = format!("refused in round {i}");
                    break;
                }
                let line = match s.local_addr() {
                    Ok(a) => format!("peer={a} reason=done"),
                    Err(e) => return Err(e.to_string()),
                };
                drop(s);
                if !ctx.wait_log(&line, Duration::from_secs(3)) {
                    out = format!("close not logged in round {i}");
                    break;
                }
            }
            out
        }
        "held_closed" => {
            // After a kill: every held connection reads end of stream or
            // an error.
            let mut open = 0;
            for (k, s) in &mut ctx.conns {
                if !k.starts_with("held") {
                    continue;
                }
                let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
                let mut b = [0u8; 4];
                if s.read(&mut b).is_ok_and(|n| n > 0) {
                    open += 1;
                }
            }
            if open == 0 {
                "all closed".into()
            } else {
                format!("{open} still answering")
            }
        }
        "elapsed" => {
            let m = ctx
                .marks
                .get(st.text.as_deref().unwrap_or(""))
                .ok_or("no such mark")?;
            timed(st, m.elapsed())
        }
        "shutdown" => yes(ctx.conn(st)?.shutdown(Shutdown::Write).is_ok()),
        "recv_eof" => {
            let t = timeout(st, 5000);
            let s = ctx.conn(st)?;
            let start = Instant::now();
            let (n, _, ended) = read_to_eof(s, t);
            if !ended {
                format!("bytes={n} no end")
            } else if st.min_ms.is_some() || st.max_ms.is_some() {
                format!("bytes={n} {}", timed(st, start.elapsed()))
            } else {
                format!("bytes={n}")
            }
        }
        "recv_reply" => {
            // An upload's reply from the hash upstream: its byte count.
            let t = timeout(st, 30_000);
            let s = ctx.conn(st)?;
            let _ = s.set_read_timeout(Some(t));
            let mut line = String::new();
            let _ = s.read_to_string(&mut line);
            let count = line.split_whitespace().nth(1).unwrap_or("none").to_string();
            match ctx.marks.get(st.text.as_deref().unwrap_or("")) {
                Some(m) => format!("count={count} {}", timed(st, m.elapsed())),
                None => format!("count={count}"),
            }
        }
        "upload" => {
            // Sends `bytes` pattern bytes and half-closes, in the
            // background; the reply is read with `recv_reply`.
            let total = st.bytes.ok_or("upload needs bytes")?;
            let mut s = ctx.conn(st)?.try_clone().map_err(|e| e.to_string())?;
            let name = st.conn.clone().unwrap_or_default();
            ctx.marks.insert(name.clone(), Instant::now());
            let job = std::thread::spawn(move || {
                let mut buf = vec![0u8; 16 * 1024];
                let mut off = 0;
                while off < total {
                    let n = (total - off).min(buf.len() as u64) as usize;
                    pattern(&mut buf[..n], off);
                    if s.write_all(&buf[..n]).is_err() {
                        return "upload cut".to_string();
                    }
                    off += n as u64;
                }
                let _ = s.shutdown(Shutdown::Write);
                "upload sent".to_string()
            });
            ctx.jobs.insert(format!("upload:{name}"), job);
            "started".into()
        }
        "download" => {
            let total = st.bytes.ok_or("download needs bytes")?;
            // The same-run control: the same bytes from the upstream itself.
            let direct = match st.max_vs_direct {
                None => None,
                Some(_) => {
                    let first = ctx.script.upstream.first().ok_or("no upstream")?;
                    let up = ctx.ups[&first.name].addr;
                    let start = Instant::now();
                    let mut s = connect_from(None, up, Duration::from_secs(3))
                        .map_err(|e| format!("connect upstream: {e}"))?;
                    let (n, _, _) = read_to_eof(&mut s, timeout(st, 30_000));
                    if n != total {
                        return Err(format!("direct download got {n} bytes"));
                    }
                    Some(start.elapsed())
                }
            };
            let start = Instant::now();
            let mut s = connect_from(st.from.as_deref(), to, Duration::from_secs(3))
                .map_err(|e| format!("connect: {e}"))?;
            let (n, h, _) = read_to_eof(&mut s, timeout(st, 30_000));
            let hash = if h == pattern_hash(total) {
                "ok"
            } else {
                "wrong"
            };
            let took = start.elapsed();
            let time = match (st.max_vs_direct, direct) {
                (Some(k), Some(d)) => {
                    let (p, d) = (took.as_millis() as u64, (d.as_millis() as u64).max(1));
                    if p <= d.saturating_mul(k) {
                        "time=ok".to_string()
                    } else {
                        format!("time={p}ms, over {k} times the direct {d}ms")
                    }
                }
                _ => timed(st, took),
            };
            format!("bytes={n} hash={hash} {time}")
        }
        "download_bg" => {
            // A download in the background, joined with `join`.
            let total = st.bytes.ok_or("download_bg needs bytes")?;
            let from = st.from.clone();
            let limit = timeout(st, 30_000);
            let name = st.conn.clone().ok_or("download_bg needs conn")?;
            let job = std::thread::spawn(move || {
                let start = Instant::now();
                match connect_from(from.as_deref(), to, Duration::from_secs(3)) {
                    Ok(mut s) => {
                        let (n, h, _) = read_to_eof(&mut s, limit);
                        let hash = if h == pattern_hash(total) {
                            "ok"
                        } else {
                            "wrong"
                        };
                        format!("bytes={n} hash={hash} ms={}", start.elapsed().as_millis())
                    }
                    Err(e) => format!("connect {e}"),
                }
            });
            ctx.jobs.insert(name, job);
            "started".into()
        }
        "join" => {
            let name = st.conn.as_deref().ok_or("join needs conn")?;
            let out = ctx
                .jobs
                .remove(name)
                .ok_or("no such job")?
                .join()
                .map_err(|_| "job panicked")?;
            // A timed job reports ms=; the step's bounds make it time=ok.
            match out.rsplit_once(" ms=") {
                Some((head, ms)) if st.min_ms.is_some() || st.max_ms.is_some() => {
                    let ms: u64 = ms.parse().unwrap_or(0);
                    format!("{head} {}", timed(st, Duration::from_millis(ms)))
                }
                Some((head, _)) => head.to_string(),
                None => out,
            }
        }
        "together" => {
            // Joins the named jobs (in `sources`) and checks the slowest
            // against min..max ms and the fastest against `count` ms.
            let mut times = Vec::new();
            let mut heads = Vec::new();
            for j in &st.sources {
                let out = ctx
                    .jobs
                    .remove(j)
                    .ok_or("no such job")?
                    .join()
                    .map_err(|_| "job panicked")?;
                let (head, ms) = out.rsplit_once(" ms=").ok_or("untimed job")?;
                times.push(ms.parse::<u64>().unwrap_or(0));
                heads.push(head.to_string());
            }
            let slow = times.iter().copied().max().unwrap_or(0);
            let fast = times.iter().copied().min().unwrap_or(0);
            let floor = st.count.unwrap_or(0);
            format!(
                "{} slowest {} fastest {}",
                heads.join(", "),
                timed(st, Duration::from_millis(slow)),
                if fast >= floor {
                    "ok".to_string()
                } else {
                    format!("{fast}ms under {floor}")
                }
            )
        }
        "trickle" => {
            // Sends `bytes` bytes every `ms`, `count` times, in the
            // background; `join` gives how many went.
            let chunk = st.bytes.ok_or("trickle needs bytes")? as usize;
            let every = Duration::from_millis(st.ms.ok_or("trickle needs ms")?);
            let rounds = st.count.ok_or("trickle needs count")?;
            let name = st.conn.clone().unwrap_or_default();
            let mut s = ctx.conn(st)?.try_clone().map_err(|e| e.to_string())?;
            let job = std::thread::spawn(move || {
                let buf = vec![b'x'; chunk];
                let mut sent = 0;
                for _ in 0..rounds {
                    if s.write_all(&buf).is_err() {
                        break;
                    }
                    sent += 1;
                    std::thread::sleep(every);
                }
                if sent == rounds {
                    "sent all".to_string()
                } else {
                    "cut before the end".to_string()
                }
            });
            ctx.jobs.insert(name, job);
            "started".into()
        }
        "closed_within" => {
            let start = ctx
                .marks
                .get(st.text.as_deref().unwrap_or(""))
                .copied()
                .unwrap_or_else(Instant::now);
            let s = ctx.conn(st)?;
            let (_, _, ended) = read_to_eof(s, timeout(st, 5000));
            if ended {
                format!("closed {}", timed(st, start.elapsed()))
            } else {
                "open".into()
            }
        }
        "flood" => {
            // Sends 64 KiB at a time for `ms`, each send giving up after
            // 200 ms, and reports whether under `bytes` went out.
            let ms = st.ms.ok_or("flood needs ms")?;
            let cap = st.bytes.ok_or("flood needs bytes")?;
            let s = ctx.conn(st)?;
            let _ = s.set_write_timeout(Some(Duration::from_millis(200)));
            let chunk = vec![0u8; 65536];
            let (start, mut sent) = (Instant::now(), 0u64);
            while start.elapsed() < Duration::from_millis(ms) {
                match s.write(&chunk) {
                    Ok(n) => sent += n as u64,
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
            if sent < cap {
                "held back".into()
            } else {
                format!("sent {sent}")
            }
        }
        "log" => {
            let needle = ctx.render(st.text.as_deref().ok_or("log needs text")?);
            if ctx.wait_log(&needle, timeout(st, 3000)) {
                "found".into()
            } else {
                "missing".into()
            }
        }
        "no_log" => {
            let needle = ctx.render(st.text.as_deref().ok_or("no_log needs text")?);
            if ctx.daemon.text().contains(&needle) {
                "present".into()
            } else {
                "absent".into()
            }
        }
        "log_count" => {
            let needle = ctx.render(st.text.as_deref().ok_or("log_count needs text")?);
            ctx.daemon.text().matches(&needle).count().to_string()
        }
        "sleep" => {
            std::thread::sleep(Duration::from_millis(st.ms.ok_or("sleep needs ms")?));
            "ok".into()
        }
        "mark" => {
            ctx.marks
                .insert(st.text.clone().unwrap_or_default(), Instant::now());
            "ok".into()
        }
        "signal" => {
            let sig = st.text.as_deref().ok_or("signal needs text")?;
            if sig != "TERM" && sig != "HUP" && sig != "KILL" {
                return Err(format!("unknown signal {sig}"));
            }
            yes(ctx.signal(sig))
        }
        "exit" => {
            let t = timeout(st, 15_000);
            let end = Instant::now() + t;
            let Some(c) = ctx.daemon.child.as_mut() else {
                return Err("no daemon".into());
            };
            loop {
                match c.try_wait() {
                    Ok(Some(status)) => {
                        let code = status.code().map_or_else(
                            || {
                                use std::os::unix::process::ExitStatusExt;
                                format!("signal {}", status.signal().unwrap_or(0))
                            },
                            |c| c.to_string(),
                        );
                        let start = ctx.marks.get(st.text.as_deref().unwrap_or(""));
                        ctx.daemon.child = None;
                        break match start {
                            Some(m) => format!("exit {code} {}", timed(st, m.elapsed())),
                            None => format!("exit {code}"),
                        };
                    }
                    Ok(None) if Instant::now() < end => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    _ => break "still running".into(),
                }
            }
        }
        "start" => {
            ctx.start()?;
            "ready".into()
        }
        "config" => {
            let text = ctx.render(st.text.as_deref().ok_or("config needs text")?);
            std::fs::write(&ctx.conf, text).map_err(|e| e.to_string())?;
            "ok".into()
        }
        "refused_now" => {
            let mut out = "accepting".to_string();
            for _ in 0..100 {
                match TcpStream::connect_timeout(&to, Duration::from_millis(500)) {
                    Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                        out = "refused".into();
                        break;
                    }
                    _ => std::thread::sleep(Duration::from_millis(10)),
                }
            }
            out
        }
        "idle_cpu" => {
            let before = ctx.cpu_secs();
            std::thread::sleep(Duration::from_millis(st.ms.unwrap_or(1000)));
            let used = ctx.cpu_secs() - before;
            if used < 0.25 {
                "idle".into()
            } else {
                format!("busy {used:.2} s")
            }
        }
        "hold_many" => {
            // Opens `count` connections and keeps those that proxy an
            // echo: "held some, not all" when at least 2 and fewer than
            // all work, as it_limits.c's descriptor test needs.
            let n = st.count.ok_or("hold_many needs count")?;
            let mut held = 0;
            for i in 0..n {
                if let Ok(mut s) = TcpStream::connect_timeout(&to, Duration::from_secs(3))
                    && works(&mut s)
                {
                    held += 1;
                    ctx.conns.insert(format!("held{i}"), s);
                }
            }
            if held >= 2 && held < n {
                "held some, not all".into()
            } else {
                format!("held {held} of {n}")
            }
        }
        "close_held" => {
            ctx.conns.retain(|k, _| !k.starts_with("held"));
            "ok".into()
        }
        "churn" => {
            // it_failures.c's churn: each source address runs `count`
            // rounds of connect then close, a third with a reset.
            let rounds = st.count.ok_or("churn needs count")?;
            let done = Arc::new(AtomicUsize::new(0));
            let hs: Vec<_> = st
                .sources
                .iter()
                .cloned()
                .map(|src| {
                    let done = Arc::clone(&done);
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_io()
                            .build();
                        for i in 0..rounds {
                            let Ok(mut s) = connect_from(Some(&src), to, Duration::from_secs(3))
                            else {
                                continue;
                            };
                            if i % 3 == 0 {
                                // Closed with SO_LINGER 0: a reset. std has
                                // no linger setter, tokio's stream does.
                                if let Ok(rt) = &rt {
                                    let _ = s.set_nonblocking(true);
                                    let _guard = rt.enter();
                                    if let Ok(t) = tokio::net::TcpStream::from_std(s) {
                                        #[allow(deprecated)]
                                        let _ = t.set_linger(Some(Duration::ZERO));
                                    }
                                }
                            } else if works(&mut s) {
                                done.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    })
                })
                .collect();
            for h in hs {
                let _ = h.join();
            }
            yes(done.load(Ordering::Relaxed) > 0)
        }
        "stats" => {
            // The named counters of the listener, after waiting up to the
            // timeout for `open` to reach 0 when it is asked for.
            let end = Instant::now() + timeout(st, 5000);
            let wait_open = st.counters.iter().any(|c| c == "open");
            loop {
                let (code, text) = ctx.ctl("stats");
                if code != 0 {
                    return Ok(format!("ctl failed: {}", text.trim()));
                }
                let all = parse_stats(&text);
                let Some(m) = all.values().next() else {
                    return Ok("no listener line".into());
                };
                if wait_open && m.get("open") != Some(&0) && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                let mut parts = Vec::new();
                for c in &st.counters {
                    match c.as_str() {
                        "balanced" => {
                            let closed: u64 =
                                CLOSES.iter().map(|k| m.get(*k).copied().unwrap_or(0)).sum();
                            let acc = m.get("accepted").copied().unwrap_or(0);
                            parts.push(format!("closed=accepted:{}", yes(closed == acc)));
                        }
                        "refused>0" => {
                            let r = m.get("rejected_per_ip_conns").copied().unwrap_or(0)
                                + m.get("rejected_per_ip_rate").copied().unwrap_or(0);
                            parts.push(format!("refused>0:{}", yes(r > 0)));
                        }
                        "accepted+refused" => {
                            let r = m.get("rejected_per_ip_conns").copied().unwrap_or(0)
                                + m.get("rejected_per_ip_rate").copied().unwrap_or(0)
                                + m.get("accepted").copied().unwrap_or(0);
                            parts.push(format!("accepted+refused={r}"));
                        }
                        name => parts.push(format!(
                            "{name}={}",
                            m.get(name).map_or("none".to_string(), u64::to_string)
                        )),
                    }
                }
                break parts.join(" ");
            }
        }
        "ctl" => {
            let (code, text) = ctx.ctl(st.text.as_deref().ok_or("ctl needs text")?);
            format!("exit {code}: {}", text.lines().next().unwrap_or("").trim())
        }
        other => return Err(format!("unknown op {other}")),
    })
}

fn yes(b: bool) -> String {
    if b { "yes" } else { "no" }.to_string()
}

/// Runs `script` against `target` in a fresh scratch directory under
/// `work`. An `Err` is a harness failure (an upstream or the daemon would
/// not start); outcomes, expected or not, are in the [`Run`].
pub fn run(target: &Target, script: &Script, work: &Path) -> Result<Run, String> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let dir = work.join(format!("{}-{}-{id}", script.name, target.name()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // Unix socket paths are capped near 104 bytes: keep this one short.
    let sock = std::env::temp_dir()
        .join(format!("pl4-{}-{id}.sock", std::process::id()))
        .to_path_buf();
    let sock = if sock.as_os_str().len() > 100 {
        PathBuf::from(format!("/tmp/pl4-{}-{id}.sock", std::process::id()))
    } else {
        sock
    };
    let mut ups = BTreeMap::new();
    for u in &script.upstream {
        ups.insert(
            u.name.clone(),
            Upstream::start(u).map_err(|e| format!("upstream {}: {e}", u.name))?,
        );
    }
    let mut ctx = Ctx {
        target,
        script,
        conf: dir.join("pacerd.conf"),
        sock: sock.clone(),
        port: 0,
        ups,
        daemon: Daemon {
            child: None,
            log: dir.join("log"),
        },
        conns: BTreeMap::new(),
        peers: BTreeMap::new(),
        jobs: BTreeMap::new(),
        marks: BTreeMap::new(),
        dir,
    };
    let mut started = Err(String::new());
    for _ in 0..5 {
        ctx.port = free_port();
        let text = ctx.render(&script.config);
        std::fs::write(&ctx.conf, text).map_err(|e| e.to_string())?;
        started = ctx.start();
        if started.is_ok() || !ctx.daemon.text().contains("cannot listen") {
            break;
        }
    }
    started.map_err(|e| {
        format!(
            "{} did not start: {e}\n{}",
            target.name(),
            ctx.daemon.text()
        )
    })?;
    let mut out = Run::default();
    for (i, st) in script.step.iter().enumerate() {
        let outcome = step(&mut ctx, st).map_err(|e| format!("step {}: {e}", i + 1))?;
        let label = match (&st.conn, &st.text) {
            (Some(c), _) => format!("{} {c}", st.op),
            (None, Some(t)) if st.op == "log" || st.op == "no_log" || st.op == "log_count" => {
                format!("{} {t:?}", st.op)
            }
            (None, Some(t)) if st.op != "config" => format!("{} {t}", st.op),
            _ => st.op.clone(),
        };
        let line = format!("{label}: {outcome}");
        if let Some(want) = &st.expect
            && *want != outcome
        {
            out.failed.push(format!(
                "step {} {label}: got {outcome}, want {want}",
                i + 1
            ));
        }
        out.transcript.push(line);
    }
    // Stop what is left: background jobs end with their connections.
    ctx.conns.clear();
    for (_, j) in std::mem::take(&mut ctx.jobs) {
        let _ = j.join();
    }
    if ctx.daemon.child.is_some() {
        ctx.signal("TERM");
        if let Some(c) = &mut ctx.daemon.child {
            let end = Instant::now() + Duration::from_secs(15);
            while Instant::now() < end && matches!(c.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    out.log = ctx.daemon.text();
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_dir_all(&ctx.dir);
    Ok(out)
}

/// What [`run_all`] found for one script.
#[derive(Debug)]
pub struct Report {
    pub file: String,
    pub runs: Vec<(String, Result<Run, String>)>,
    /// The first difference between the first two targets' transcripts.
    pub divergence: Option<String>,
}

impl Report {
    /// Whether every run started, met every expectation, and the runs
    /// agree.
    pub fn ok(&self) -> bool {
        self.divergence.is_none()
            && self
                .runs
                .iter()
                .all(|(_, r)| r.as_ref().is_ok_and(|r| r.failed.is_empty()))
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = if self.ok() { "ok" } else { "FAILED" };
        writeln!(f, "{}: {status}", self.file)?;
        for (name, r) in &self.runs {
            match r {
                Err(e) => writeln!(f, "  {name}: harness error: {e}")?,
                Ok(r) => {
                    for line in &r.failed {
                        writeln!(f, "  {name}: {line}")?;
                    }
                    if !r.failed.is_empty() {
                        let tail: Vec<&str> = r.log.lines().rev().take(20).collect();
                        for l in tail.iter().rev() {
                            writeln!(f, "    | {l}")?;
                        }
                    }
                }
            }
        }
        if let Some(d) = &self.divergence {
            writeln!(f, "  divergence at {d}")?;
        }
        Ok(())
    }
}

/// Runs each script against each target, in order, and diffs the first
/// two targets' transcripts. Scripts that need the loopback range are
/// refused on a host without it, as a harness error, never passed over.
pub fn run_all(targets: &[Target], scripts: &[(String, Script)], work: &Path) -> Vec<Report> {
    let range = has_loopback_range();
    scripts
        .iter()
        .map(|(file, s)| {
            let runs: Vec<(String, Result<Run, String>)> = targets
                .iter()
                .map(|t| {
                    let r = if s.needs_loopback_range && !range {
                        Err("needs loopback addresses beyond 127.0.0.1 (Linux)".into())
                    } else {
                        run(t, s, work)
                    };
                    (t.name().to_string(), r)
                })
                .collect();
            let divergence = match (runs.first(), runs.get(1)) {
                (Some((an, Ok(a))), Some((bn, Ok(b)))) => {
                    diff(an, &a.transcript, bn, &b.transcript)
                }
                _ => None,
            };
            Report {
                file: file.clone(),
                runs,
                divergence,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_lines_parse_by_listener() {
        let s = parse_stats(
            "pacerd version=0.1 uptime_s=3 workers=2 draining=0\nlistener=web open=1 accepted=4 bytes_up=9\nworker=0 accepted=4\n",
        );
        assert_eq!(s.len(), 1);
        assert_eq!(s["web"]["open"], 1);
        assert_eq!(s["web"]["accepted"], 4);
        assert!(!s["web"].contains_key("listener"));
    }

    #[test]
    fn diff_names_the_first_differing_step() {
        let a = vec!["works a: yes".to_string(), "log x: found".to_string()];
        let b = vec!["works a: yes".to_string(), "log x: missing".to_string()];
        let d = diff("pacerd", &a, "edgefront", &b).unwrap();
        assert!(d.starts_with("step 2: pacerd: log x: found"), "{d}");
        assert!(diff("p", &a, "e", &a).is_none());
        let short = diff("p", &a, "e", &a[..1]).unwrap();
        assert!(short.contains("(none)"));
    }

    #[test]
    fn bounds_read_as_time_ok() {
        let st = Step {
            min_ms: Some(10),
            max_ms: Some(20),
            ..Step::default()
        };
        assert_eq!(timed(&st, Duration::from_millis(15)), "time=ok");
        assert_eq!(
            timed(&st, Duration::from_millis(25)),
            "time=25ms, outside 10..20"
        );
        assert_eq!(timed(&Step::default(), Duration::from_secs(9)), "time=ok");
    }

    #[test]
    fn the_pattern_hash_covers_whole_and_partial_chunks() {
        let mut buf = vec![0u8; 70_000];
        pattern(&mut buf, 0);
        assert_eq!(pattern_hash(70_000), fnv(FNV_START, &buf));
        assert_eq!(pattern_hash(0), FNV_START);
    }
}
