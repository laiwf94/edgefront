//! `edgefront limits` as a process: the binary started on a pacerd.conf,
//! driven over real sockets and signals, its log and counters read the
//! way pacer's `tests/harness.c` read pacerd's. These cover descriptor
//! exhaustion and a full client table (pacerd's `it_limits.c`), shaping
//! held within 2% over 30 seconds, and a drain of 10,000 connections that
//! loses no byte. The other `it_*.c` scenarios run against both binaries
//! as traffic scripts (`tests/pacer_scripts`).

use std::io::{Read, Write as _};
use std::net::{SocketAddr, TcpListener as StdListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const BIN: &str = env!("CARGO_BIN_EXE_edgefront");
const KIB: u64 = 1024;

/// A directory under the system temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "efl-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A running `edgefront limits`, its stderr written to a file.
struct Proc {
    child: Child,
    log: PathBuf,
    port: u16,
    _dir: Scratch,
}

fn free_port() -> u16 {
    StdListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Proc {
    /// Starts the binary on a config whose one listener `lim` listens on a
    /// free port of `host` and sends to `upstream`, with `extra` lines in
    /// the listener and `top` lines before it. With `nofile`, the process
    /// gets that descriptor limit. Waits for `msg=ready`.
    fn start(
        host: &str,
        upstream: SocketAddr,
        top: &str,
        extra: &str,
        nofile: Option<u32>,
    ) -> Self {
        for _ in 0..5 {
            let dir = Scratch::new();
            let port = free_port();
            let conf = dir.0.join("pacerd.conf");
            let listen = if host.contains(':') {
                format!("[{host}]:{port}")
            } else {
                format!("{host}:{port}")
            };
            std::fs::write(
                &conf,
                format!(
                    "{top}listener lim {{\n  listen {listen}\n  upstream {upstream}\n{extra}}}\n"
                ),
            )
            .unwrap();
            let log = dir.0.join("log");
            let err = std::fs::File::create(&log).unwrap();
            let mut cmd = Command::new("sh");
            let limit = nofile.map_or(String::new(), |n| format!("ulimit -n {n} && "));
            cmd.arg("-c")
                .arg(format!("{limit}exec \"$0\" limits -c \"$1\""))
                .arg(BIN)
                .arg(&conf)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(err);
            let child = cmd.spawn().unwrap();
            let mut p = Proc {
                child,
                log,
                port,
                _dir: dir,
            };
            if p.wait_log("msg=ready", Duration::from_secs(10)) {
                return p;
            }
            let text = p.log_text();
            let _ = p.child.kill();
            let _ = p.child.wait();
            assert!(text.contains("cannot listen"), "did not start:\n{text}");
        }
        panic!("no free port after 5 tries");
    }

    fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn wait_log(&mut self, needle: &str, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if self.log_text().contains(needle) {
                return true;
            }
            if let Ok(Some(_)) = self.child.try_wait() {
                return self.log_text().contains(needle);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn signal(&self, sig: &str) {
        let ok = Command::new("kill")
            .arg(format!("-{sig}"))
            .arg(self.child.id().to_string())
            .status()
            .unwrap()
            .success();
        assert!(ok, "kill -{sig}");
    }

    /// Sends SIGTERM and waits for the exit code.
    fn stop(mut self) -> (i32, String) {
        self.signal("TERM");
        let code = self.wait_exit(Duration::from_secs(30));
        (code, self.log_text())
    }

    fn wait_exit(&mut self, timeout: Duration) -> i32 {
        let end = Instant::now() + timeout;
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st.code().unwrap_or(-1);
            }
            if Instant::now() > end {
                let _ = self.child.kill();
                panic!("did not exit:\n{}", self.log_text());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// CPU seconds the process has used: /proc on Linux, ps elsewhere.
    fn cpu_secs(&self) -> f64 {
        let pid = self.child.id();
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            // utime and stime are fields 14 and 15, after the ")" of the
            // name, in ticks of 1/100 s.
            let rest = &stat[stat.rfind(')').unwrap() + 2..];
            let f: Vec<&str> = rest.split(' ').collect();
            let ticks: f64 = f[11].parse::<f64>().unwrap() + f[12].parse::<f64>().unwrap();
            return ticks / 100.0;
        }
        let out = Command::new("ps")
            .args(["-o", "time=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        // macOS: [hh:]mm:ss.cc
        let t = String::from_utf8(out.stdout).unwrap();
        t.trim()
            .split(':')
            .fold(0.0, |acc, part| acc * 60.0 + part.parse::<f64>().unwrap())
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An upstream on `addr` serving every connection with `serve`.
async fn upstream<F, Fut>(addr: &str, serve: F) -> SocketAddr
where
    F: Fn(TcpStream) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let l = TcpListener::bind(addr).await.unwrap();
    let a = l.local_addr().unwrap();
    let serve = Arc::new(serve);
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            tokio::spawn(serve(s));
        }
    });
    a
}

async fn echo(mut s: TcpStream) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match s.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if s.write_all(&buf[..n]).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// pacer's test pattern: byte `i` of a stream is a function of `i`.
fn pattern(buf: &mut [u8], off: u64) {
    for (i, b) in buf.iter_mut().enumerate() {
        let x = off + i as u64;
        *b = (x ^ (x >> 8) ^ (x >> 16)) as u8;
    }
}

/// Sends `total` pattern bytes, then closes.
async fn source(mut s: TcpStream, total: u64) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut off = 0;
    while off < total {
        let n = (total - off).min(buf.len() as u64) as usize;
        pattern(&mut buf[..n], off);
        if s.write_all(&buf[..n]).await.is_err() {
            return;
        }
        off += n as u64;
    }
    let _ = s.shutdown().await;
}

/// Whether a connection proxies an echo.
async fn works(s: &mut TcpStream) -> bool {
    if s.write_all(b"ping").await.is_err() {
        return false;
    }
    let mut b = [0u8; 4];
    matches!(
        tokio::time::timeout(Duration::from_secs(3), s.read_exact(&mut b)).await,
        Ok(Ok(_))
    ) && &b == b"ping"
}

/// Whether the proxy closed a connection without sending anything.
async fn refused(s: &mut TcpStream) -> bool {
    let mut b = [0u8; 1];
    matches!(
        tokio::time::timeout(Duration::from_secs(3), s.read(&mut b)).await,
        Ok(Ok(0) | Err(_))
    )
}

// it_limits.c: limits_fd_exhaustion_sheds_without_spinning. With 32
// descriptors the daemon has room for a few connections (two descriptors
// each); further clients are accepted and closed at once instead of left
// hanging in the queue, the process stays idle while one waits, and once
// the held connections close new ones work again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_descriptors_new_connections_are_closed_at_once_without_spinning() {
    let up = upstream("127.0.0.1:0", echo).await;
    let p = Proc::start("127.0.0.1", up, "workers 1\n", "", Some(32));
    let mut held = Vec::new();
    for _ in 0..24 {
        let mut s = TcpStream::connect(p.addr()).await.unwrap();
        if works(&mut s).await {
            held.push(s);
        }
    }
    assert!(held.len() >= 2, "{} held", held.len());
    assert!(held.len() < 24, "{} held", held.len());
    let mut p = p;
    assert!(
        p.wait_log("reason=fd_exhausted", Duration::from_secs(3)),
        "{}",
        p.log_text()
    );
    let mut pending = TcpStream::connect(p.addr()).await.unwrap();
    let before = p.cpu_secs();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let after = p.cpu_secs();
    assert!(refused(&mut pending).await);
    // Less than a quarter of one CPU over that second.
    assert!(after - before < 0.25, "{before} then {after}");
    drop(held);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut s = TcpStream::connect(p.addr()).await.unwrap();
    assert!(works(&mut s).await);
    drop(s);
    let (code, log) = p.stop();
    assert_eq!(code, 0, "{log}");
    assert!(log.contains("msg=drained"));
}

/// Connects to `to` from the loopback address `from`.
#[cfg(target_os = "linux")]
async fn connect_from(from: [u8; 4], to: SocketAddr) -> TcpStream {
    let s = tokio::net::TcpSocket::new_v4().unwrap();
    s.bind(SocketAddr::from((from, 0))).unwrap();
    s.connect(to).await.unwrap()
}

// it_limits.c's table-full case, as pacerd's README states it: with the
// client table full of clients that have connections open, a new address
// is refused at once as table_full, and the clients already tracked keep
// their limits. It needs 17 loopback addresses, which Linux has and macOS
// does not (only 127.0.0.1 is configured there).
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_client_table_refuses_new_addresses_and_keeps_old_limits() {
    let up = upstream("127.0.0.1:0", echo).await;
    let mut p = Proc::start(
        "127.0.0.1",
        up,
        "workers 2\n",
        "  table_size 16\n  table_full deny\n  per_ip_max_conns 2\n",
        None,
    );
    let mut held = Vec::new();
    for i in 0..16u8 {
        let mut s = connect_from([127, 0, 1, i + 1], p.addr()).await;
        assert!(works(&mut s).await, "client {i}");
        held.push(s);
    }
    // A new address: refused at once, with nothing read.
    let t = Instant::now();
    let mut new = connect_from([127, 0, 2, 1], p.addr()).await;
    assert!(refused(&mut new).await);
    assert!(t.elapsed() < Duration::from_secs(1));
    assert!(p.wait_log("reason=table_full", Duration::from_secs(3)));
    // A tracked client keeps its cap: one more works, the third is refused.
    let mut second = connect_from([127, 0, 1, 1], p.addr()).await;
    assert!(works(&mut second).await);
    let mut third = connect_from([127, 0, 1, 1], p.addr()).await;
    assert!(refused(&mut third).await);
    assert!(p.wait_log("reason=per_ip_conns", Duration::from_secs(3)));
    // Once a client has nothing open its entry can be reclaimed, and the
    // new address gets in.
    drop(held.remove(5));
    assert!(p.wait_log("peer=127.0.1.6:", Duration::from_secs(3)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut late = connect_from([127, 0, 2, 1], p.addr()).await;
    assert!(works(&mut late).await);
    drop((held, second, late));
    let (code, log) = p.stop();
    assert_eq!(code, 0, "{log}");
    assert!(log.contains("rejected_table_full") || log.contains("reason=table_full"));
}

/// Done when, item 2: one client held to `per_ip_bandwidth` over 30
/// seconds. A download of 15 MiB plus the 64 KiB burst at 512 KiB/s takes
/// 30 s; it must take that within 2%.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shaping_holds_a_download_to_its_rate_within_two_percent_over_30_seconds() {
    const RATE: u64 = 512 * KIB;
    const BURST: u64 = 64 * KIB;
    const TOTAL: u64 = 30 * RATE + BURST;
    let up = upstream("127.0.0.1:0", |s| source(s, TOTAL)).await;
    let p = Proc::start(
        "127.0.0.1",
        up,
        "",
        "  per_ip_bandwidth 512k/s burst 64k\n",
        None,
    );
    let mut s = TcpStream::connect(p.addr()).await.unwrap();
    let start = Instant::now();
    let mut buf = vec![0u8; 64 * 1024];
    let mut got = 0u64;
    let mut want = vec![0u8; 64 * 1024];
    loop {
        let n = s.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        pattern(&mut want[..n], got);
        assert_eq!(&buf[..n], &want[..n], "at byte {got}");
        got += n as u64;
    }
    let took = start.elapsed().as_secs_f64();
    assert_eq!(got, TOTAL);
    let achieved = (TOTAL - BURST) as f64 / took;
    assert!(
        (achieved - RATE as f64).abs() / RATE as f64 <= 0.02,
        "{achieved:.0} B/s over {took:.2} s"
    );
    drop(s);
    let mut p = p;
    let line = format!("reason=done bytes_up=0 bytes_down={TOTAL}");
    assert!(
        p.wait_log(&line, Duration::from_secs(3)),
        "{}",
        p.log_text()
    );
    let (code, log) = p.stop();
    assert_eq!(code, 0, "{log}");
}

/// Runs the test `name` of this binary again in a child with a descriptor
/// limit of `nofile`, returning true in the child; the parent requires the
/// child to pass. Raising a limit needs no unsafe code this way.
fn in_child_with_nofile(name: &str, nofile: u32) -> bool {
    const CHILD: &str = "EDGEFRONT_LIMITS_NOFILE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let exe = std::env::current_exe().unwrap();
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "ulimit -n {nofile} && exec \"$0\" --exact {name} --nocapture --test-threads 1"
        ))
        .arg(exe)
        .env(CHILD, "1")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "child failed:\n{text}");
    assert!(text.contains("1 passed"), "{text}");
    false
}

/// Done when, item 4: SIGTERM drains 10,000 open connections with zero
/// bytes lost. Each download is 16 KiB: the upstream sends the first half,
/// waits until SIGTERM has been sent and new connections are refused, then
/// sends the rest and closes. Every client must get every byte in order,
/// and the daemon must exit 0 after `drained`, with no drain timeout.
#[test]
fn sigterm_drains_10000_connections_without_losing_a_byte() {
    const NAME: &str = "sigterm_drains_10000_connections_without_losing_a_byte";
    if !in_child_with_nofile(NAME, 32_768) {
        return;
    }
    const N: usize = 10_000;
    const EACH: u64 = 16 * KIB;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let gone = Arc::new(AtomicUsize::new(0));
        let accepted = Arc::new(AtomicUsize::new(0));
        let a = Arc::clone(&accepted);
        let signalled = tokio::sync::watch::channel(false).0;
        let seen = signalled.subscribe();
        let up = upstream("[::1]:0", move |mut s: TcpStream| {
            let a = Arc::clone(&a);
            let mut seen = seen.clone();
            async move {
                a.fetch_add(1, Ordering::Relaxed);
                let mut buf = vec![0u8; EACH as usize];
                pattern(&mut buf, 0);
                let half = buf.len() / 2;
                if s.write_all(&buf[..half]).await.is_err() {
                    return;
                }
                let _ = seen.wait_for(|v| *v).await;
                tokio::time::sleep(Duration::from_millis(200)).await;
                let _ = s.write_all(&buf[half..]).await;
                let _ = s.shutdown().await;
            }
        })
        .await;
        // Daemon and upstream talk over IPv6 so the two legs do not share
        // one ephemeral port range.
        let p = Proc::start(
            "127.0.0.1",
            up,
            "workers 4\ndrain_timeout 60s\n",
            "  max_conns 20000\n  buffer 4k\n",
            Some(32_768),
        );
        let addr = p.addr();
        let done = Arc::new(AtomicU64::new(0));
        let mut clients = Vec::with_capacity(N);
        for i in 0..N {
            let s = loop {
                match TcpStream::connect(addr).await {
                    Ok(s) => break s,
                    Err(e) => {
                        assert!(i > 0, "{e}");
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
            };
            let (done, gone) = (Arc::clone(&done), Arc::clone(&gone));
            clients.push(tokio::spawn(async move {
                let mut s = s;
                let mut got = Vec::with_capacity(EACH as usize);
                match s.read_to_end(&mut got).await {
                    Ok(_) => {}
                    Err(_) => {
                        gone.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                }
                let mut want = vec![0u8; EACH as usize];
                pattern(&mut want, 0);
                if got == want {
                    done.fetch_add(1, Ordering::Relaxed);
                } else {
                    gone.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        // Every connection is open end to end before the signal.
        let t = Instant::now();
        while accepted.load(Ordering::Relaxed) < N {
            assert!(
                t.elapsed() < Duration::from_secs(60),
                "{} upstream",
                accepted.load(Ordering::Relaxed)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        p.signal("TERM");
        let mut p = p;
        assert!(p.wait_log("msg=draining reason=SIGTERM", Duration::from_secs(5)));
        // New connections are refused at once while the old ones finish.
        let mut refused_now = false;
        for _ in 0..100 {
            if TcpStream::connect(addr).await.is_err() {
                refused_now = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(refused_now, "still accepting while draining");
        signalled.send_replace(true);
        for c in clients {
            c.await.unwrap();
        }
        assert_eq!(gone.load(Ordering::Relaxed), 0);
        assert_eq!(done.load(Ordering::Relaxed), N as u64);
        let code = p.wait_exit(Duration::from_secs(60));
        let log = p.log_text();
        assert_eq!(code, 0, "{}", &log[log.len().saturating_sub(2000)..]);
        assert!(log.contains("msg=drained"));
        assert!(!log.contains("drain timeout"));
        assert_eq!(
            log.matches(&format!("reason=done bytes_up=0 bytes_down={EACH}"))
                .count(),
            N
        );
    });
}

// it_lifecycle.c: drain_timeout_closes_stragglers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_drain_timeout_closes_what_is_still_open() {
    let up = upstream("127.0.0.1:0", echo).await;
    let p = Proc::start("127.0.0.1", up, "drain_timeout 1s\n", "", None);
    let mut s = TcpStream::connect(p.addr()).await.unwrap();
    assert!(works(&mut s).await);
    let t = Instant::now();
    let (code, log) = p.stop();
    let ms = t.elapsed();
    assert_eq!(code, 0, "{log}");
    assert!(
        ms >= Duration::from_millis(900) && ms < Duration::from_secs(3),
        "{ms:?}"
    );
    assert!(
        log.contains("msg=\"drain timeout\" worker=0 closing=1"),
        "{log}"
    );
    assert!(log.contains("reason=drain_timeout"));
    assert!(log.contains("msg=drained"));
}

#[test]
fn check_mode_and_usage_follow_pacerd() {
    let dir = Scratch::new();
    let conf = dir.0.join("p.conf");
    std::fs::write(
        &conf,
        "listener a {\n listen 127.0.0.1:1\n upstream 127.0.0.1:2\n}\n",
    )
    .unwrap();
    let out = Command::new(BIN)
        .args(["limits", "-c", conf.to_str().unwrap(), "-t"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("edgefront: {} ok, 1 listener\n", conf.display())
    );
    std::fs::write(&conf, "listener a {\n").unwrap();
    let out = Command::new(BIN)
        .args(["limits", "-t", "-c", conf.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("line 1: listener a: missing }"));
    for args in [
        &["limits"][..],
        &["limits", "-x"],
        &["ctl"],
        &["ctl", "limits"],
        &["ctl", "limits", "bogus"],
    ] {
        let out = Command::new(BIN).args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).starts_with("usage: edgefront"));
    }
    let out = Command::new(BIN).args(["limits", "-h"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    // A bad config at start is exit 2 with pacerd's log line.
    let out = Command::new(BIN)
        .args(["limits", "-c", conf.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("msg=\"bad config\""));
    // A port already taken: exit 1.
    let taken = StdListener::bind("127.0.0.1:0").unwrap();
    std::fs::write(
        &conf,
        format!(
            "listener a {{\n listen {}\n upstream 127.0.0.1:2\n}}\n",
            taken.local_addr().unwrap()
        ),
    )
    .unwrap();
    let out = Command::new(BIN)
        .args(["limits", "-c", conf.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("msg=\"cannot listen\""));
}

/// Runs `edgefront ctl limits -s SOCK cmd`.
fn ctl(sock: &Path, cmd: &str) -> (i32, String, String) {
    let out = Command::new(BIN)
        .args(["ctl", "limits", "-s", sock.to_str().unwrap(), cmd])
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A short socket path: macOS caps Unix socket paths at 104 bytes.
fn short_socket() -> (PathBuf, impl Drop) {
    struct Rm(PathBuf);
    impl Drop for Rm {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    static N: AtomicUsize = AtomicUsize::new(0);
    let p = PathBuf::from(format!(
        "/tmp/efl-{}-{}.sock",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    (p.clone(), Rm(p))
}

// it_admin.c, through the HTTP admin API: ping, stats, reload and drain,
// a stale socket file replaced and a live one refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_admin_socket_answers_pacerctls_commands() {
    let up = upstream("127.0.0.1:0", echo).await;
    let (sock, _rm) = short_socket();
    // A stale socket file from a crashed run is replaced.
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
    let top = format!("admin_socket {}\n", sock.display());
    let mut p = Proc::start("127.0.0.1", up, &top, "  per_ip_max_conns 1\n", None);
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(ctl(&sock, "ping"), (0, "ok pong\n".into(), String::new()));
    let mut a = TcpStream::connect(p.addr()).await.unwrap();
    assert!(works(&mut a).await);
    let mut b = TcpStream::connect(p.addr()).await.unwrap();
    assert!(refused(&mut b).await);
    let (code, stats, _) = ctl(&sock, "stats");
    assert_eq!(code, 0);
    let lines: Vec<&str> = stats.lines().collect();
    assert!(lines[0].starts_with("edgefront version="), "{stats}");
    assert!(lines[0].ends_with(" workers=1 draining=0"), "{stats}");
    assert!(
        lines[1].starts_with(
            "listener=lim open=1 accepted=1 rejected_listener_full=0 rejected_per_ip_conns=1 "
        ),
        "{stats}"
    );
    assert!(lines[1].contains(" bytes_up=4 bytes_down=4 "), "{stats}");
    assert!(
        lines[1]
            .ends_with(" clients_tracked=1 client_table_capacity=16384 client_table_evictions=0")
    );
    // A second daemon on the same socket is refused while this one lives.
    let up2 = upstream("127.0.0.1:0", echo).await;
    let dir = Scratch::new();
    let conf = dir.0.join("p.conf");
    std::fs::write(
        &conf,
        format!(
            "admin_socket {}\nlistener b {{\n listen 127.0.0.1:{}\n upstream {up2}\n}}\n",
            sock.display(),
            free_port()
        ),
    )
    .unwrap();
    let out = Command::new(BIN)
        .args(["limits", "-c", conf.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("admin_socket is in use"));
    // A reload that cannot apply is an error, and the old config stays.
    let conf_path = p.log.parent().unwrap().join("pacerd.conf");
    let good = std::fs::read_to_string(&conf_path).unwrap();
    std::fs::write(
        &conf_path,
        good.replace("workers", "x")
            .replace("listener lim", "workers 3\nlistener lim"),
    )
    .unwrap();
    let (code, _, err) = ctl(&sock, "reload");
    assert_eq!(code, 1);
    assert_eq!(err, "error workers cannot change without a restart\n");
    std::fs::write(
        &conf_path,
        good.replace("per_ip_max_conns 1", "per_ip_max_conns 2"),
    )
    .unwrap();
    assert_eq!(
        ctl(&sock, "reload"),
        (0, "ok reloaded\n".into(), String::new())
    );
    assert!(p.wait_log("msg=reloaded source=admin", Duration::from_secs(3)));
    let mut c = TcpStream::connect(p.addr()).await.unwrap();
    assert!(works(&mut c).await);
    drop((a, c));
    assert_eq!(
        ctl(&sock, "drain"),
        (0, "ok draining\n".into(), String::new())
    );
    assert!(p.wait_log("msg=draining reason=admin", Duration::from_secs(3)));
    assert_eq!(p.wait_exit(Duration::from_secs(10)), 0);
    // The socket is removed on exit, and a client then cannot connect.
    assert!(!sock.exists());
    let (code, _, err) = ctl(&sock, "ping");
    assert_eq!(code, 1);
    assert!(err.contains("cannot connect to"), "{err}");
}

#[test]
fn an_unknown_admin_path_or_method_is_refused() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let up = upstream("127.0.0.1:0", echo).await;
        let (sock, _rm) = short_socket();
        let top = format!("admin_socket {}\n", sock.display());
        let p = Proc::start("127.0.0.1", up, &top, "", None);
        for (req, want) in [
            ("GET /nope HTTP/1.1\r\nhost: x\r\n\r\n", "404"),
            (
                "POST /ping HTTP/1.1\r\nhost: x\r\ncontent-length: 0\r\n\r\n",
                "405",
            ),
            ("GET /reload HTTP/1.1\r\nhost: x\r\n\r\n", "405"),
        ] {
            let mut s = std::os::unix::net::UnixStream::connect(&sock).unwrap();
            s.write_all(req.as_bytes()).unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            assert!(out.starts_with(&format!("HTTP/1.1 {want}")), "{out}");
        }
        // A client that sends nothing is dropped after its deadline.
        let t = Instant::now();
        let mut s = std::os::unix::net::UnixStream::connect(&sock).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        assert!(t.elapsed() < Duration::from_secs(12));
        let (code, log) = p.stop();
        assert_eq!(code, 0, "{log}");
    });
}

#[test]
fn a_ninth_admin_client_is_told_busy_and_a_path_that_is_not_a_socket_is_refused() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let up = upstream("127.0.0.1:0", echo).await;
        let (sock, _rm) = short_socket();
        let top = format!("admin_socket {}\n", sock.display());
        let p = Proc::start("127.0.0.1", up, &top, "", None);
        // Eight clients that send nothing take every slot.
        let held: Vec<_> = (0..8)
            .map(|_| std::os::unix::net::UnixStream::connect(&sock).unwrap())
            .collect();
        std::thread::sleep(Duration::from_millis(200));
        let mut ninth = std::os::unix::net::UnixStream::connect(&sock).unwrap();
        let mut out = String::new();
        ninth.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 503"), "{out}");
        assert!(out.ends_with("\r\n\r\nerror busy\n"), "{out}");
        // Once their deadline passes the slots are free again.
        drop(held);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(ctl(&sock, "ping").1, "ok pong\n");
        let (code, log) = p.stop();
        assert_eq!(code, 0, "{log}");
    });
    // A file that is not a socket is never removed or replaced.
    let dir = Scratch::new();
    let (sock, _rm) = short_socket();
    std::fs::write(&sock, "not a socket").unwrap();
    let conf = dir.0.join("p.conf");
    std::fs::write(
        &conf,
        format!(
            "admin_socket {}\nlistener a {{\n listen 127.0.0.1:{}\n upstream 127.0.0.1:2\n}}\n",
            sock.display(),
            free_port()
        ),
    )
    .unwrap();
    let out = Command::new(BIN)
        .args(["limits", "-c", conf.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("msg=\"admin_socket path exists and is not a socket\""),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&sock).unwrap(), "not a socket");
}

// pacerctl had no way to see who holds a listener's connections; the
// admin API lists the busiest clients and looks one address up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctl_lists_the_busiest_clients_and_looks_one_up() {
    let up = upstream("127.0.0.1:0", echo).await;
    let (sock, _rm) = short_socket();
    let top = format!("admin_socket {}\n", sock.display());
    let p = Proc::start(
        "127.0.0.1",
        up,
        &top,
        "  per_ip_max_conns 5\n  per_ip_conn_rate 10/s burst 10\n  per_ip_bandwidth 1m/s burst 64k\n",
        None,
    );
    let mut held = Vec::new();
    for _ in 0..3 {
        let mut s = TcpStream::connect(p.addr()).await.unwrap();
        assert!(works(&mut s).await);
        held.push(s);
    }
    let (code, out, _) = ctl(&sock, "top");
    assert_eq!(code, 0);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "listener=lim clients_tracked=1 shown=1", "{out}");
    assert!(
        lines[1].starts_with("client=127.0.0.1 tracked=yes open=3 conn_tokens="),
        "{out}"
    );
    // Tokens refill as time passes, so only their presence is fixed.
    assert!(lines[1].contains(" byte_tokens="), "{out}");
    assert!(lines[1].ends_with(" quota_used=-"), "{out}");
    let out = Command::new(BIN)
        .args([
            "ctl",
            "limits",
            "-s",
            sock.to_str().unwrap(),
            "-l",
            "lim",
            "client",
            "127.0.0.1",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("listener=lim client=127.0.0.1 tracked=yes open=3 "),
        "{text}"
    );
    let (code, text, _) = ctl(&sock, "stats");
    assert_eq!(code, 0);
    assert!(text.contains("listener=lim open=3 accepted=3 "), "{text}");
    // An address nobody used, a listener that does not exist, bad input.
    let out = Command::new(BIN)
        .args([
            "ctl",
            "limits",
            "-s",
            sock.to_str().unwrap(),
            "client",
            "192.0.2.9",
        ])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).starts_with(
        "listener=lim client=192.0.2.9 tracked=no open=0 conn_tokens=10 byte_tokens=65536 quota_used=-"
    ));
    for (args, want) in [
        (vec!["-l", "nope", "top"], "error no listener nope\n"),
        (
            vec!["client", "not-an-ip"],
            "error not an IP address: not-an-ip\n",
        ),
    ] {
        let mut a = vec!["ctl", "limits", "-s", sock.to_str().unwrap()];
        a.extend(args);
        let out = Command::new(BIN).args(&a).output().unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(String::from_utf8_lossy(&out.stderr), want);
    }
    let out = Command::new(BIN)
        .args(["ctl", "limits", "-s", sock.to_str().unwrap(), "top", "x"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    drop(held);
    let (code, log) = p.stop();
    assert_eq!(code, 0, "{log}");
}
