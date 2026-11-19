//! End to end: `edgefront proxy` terminates TLS, picks certificates by
//! exact and wildcard SNI, and reloads them on SIGHUP while 1,000 requests
//! a second run through it, without a failed request. Against the Rust
//! binary only: this is the property legacy/steer is deleted on.
//!
//! The load is open loop: request `i` is sent at `start + i ms` whatever
//! the others are doing. A quarter of the requests open a new TLS
//! connection, cycling through an exact name, a wildcard name and a name
//! nothing covers, and the rest go over keep-alive connections opened
//! before the reload. Halfway through, the file is rewritten to rotate the
//! exact name's certificate and the process gets SIGHUP. Every request
//! must succeed; new connections must see the old certificate before the
//! reload and the rotated one after it; the keep-alive connections must
//! keep the certificate they negotiated.

#[path = "../../edgefront-core/tests/common/tls.rs"]
mod tlsc;

use std::convert::Infallible;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

const EDGEFRONT: &str = env!("CARGO_BIN_EXE_edgefront");
const TIMEOUT: Duration = Duration::from_secs(10);
/// Requests per second, and how long the load runs.
const RATE: u64 = 1_000;
const SECONDS: u64 = 4;

/// A running `edgefront proxy`, stopped with SIGTERM when dropped.
struct Proc {
    child: Child,
    proxy: SocketAddr,
    lines: mpsc::Receiver<String>,
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            self.signal("TERM");
            let deadline = Instant::now() + TIMEOUT;
            while Instant::now() < deadline {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Proc {
    fn signal(&self, name: &str) {
        let ok = Command::new("kill")
            .args([&format!("-{name}"), &self.child.id().to_string()])
            .status()
            .unwrap()
            .success();
        assert!(ok, "kill -{name}");
    }

    fn wait_log(&self, needle: &str) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if line.contains(needle) => return line,
                Ok(_) => {}
                Err(_) => panic!("no log line with {needle}"),
            }
        }
    }
}

fn start(config: &Path) -> Proc {
    let mut child = Command::new(EDGEFRONT)
        .args(["proxy", "--config"])
        .arg(config)
        .env_remove("EDGEFRONT_ADMIN_TOKEN")
        .env("EDGEFRONT_DRAIN_PERIOD", "0s")
        .env("EDGEFRONT_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start the proxy");
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut proc = Proc {
        child,
        proxy: "0.0.0.0:0".parse().unwrap(),
        lines,
    };
    let line = proc.wait_log("\"listening\"");
    assert!(line.contains("\"tls\":true"), "{line}");
    let key = "\"proxy\":\"";
    let at = line.find(key).unwrap() + key.len();
    proc.proxy = line[at..].split('"').next().unwrap().parse().unwrap();
    proc
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("e2e-tls-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes the configuration: one route to `upstream`, and a `tls` block
/// with `exact` (the exact name's certificate), the wildcard one and the
/// default first, by paths relative to the file.
fn write_config(path: &Path, upstream: SocketAddr, exact: &str) {
    let js = format!(
        r#"{{"listen":"127.0.0.1:0","admin_listen":"127.0.0.1:0",
        "routes":[{{"cluster":"web"}}],
        "clusters":[{{"name":"web","endpoints":[{{"address":"{upstream}"}}]}}],
        "tls":{{"certificates":[
            {{"cert_file":"default.crt","key_file":"default.key"}},
            {{"cert_file":"{exact}.crt","key_file":"{exact}.key"}},
            {{"cert_file":"wild.crt","key_file":"wild.key"}}]}}}}"#
    );
    std::fs::write(path, js).unwrap();
}

/// An upstream answering every request with its `X-Forwarded-Proto`.
async fn upstream() -> SocketAddr {
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = socket.accept().await {
            let _ = tcp.set_nodelay(true);
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(|req: http::Request<Incoming>| async move {
                    let proto = req
                        .headers()
                        .get("x-forwarded-proto")
                        .map(|v| Bytes::copy_from_slice(v.as_bytes()))
                        .unwrap_or_default();
                    Ok::<_, Infallible>(http::Response::new(Full::new(proto)))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tcp), svc)
                    .await;
            });
        }
    });
    addr
}

/// One new connection: its SNI, when its handshake started and ended,
/// and the certificate it got.
struct Handshake {
    sni: &'static str,
    started: Instant,
    ended: Instant,
    cert: &'static str,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sni_and_a_reload_under_1000_requests_a_second() {
    let dir = scratch("reload");
    for base in ["default", "exact", "rotated", "wild"] {
        for ext in ["crt", "key"] {
            std::fs::copy(
                tlsc::fixture(&format!("{base}.{ext}")),
                dir.join(format!("{base}.{ext}")),
            )
            .unwrap();
        }
    }
    let up = upstream().await;
    let cfg = dir.join("proxy.json");
    write_config(&cfg, up, "exact");
    let proc = start(&cfg);
    let addr = proc.proxy;

    let verified = tlsc::ClientOpts::default();
    let unverified = tlsc::ClientOpts {
        roots: None,
        ..tlsc::ClientOpts::default()
    };
    // Keep-alive connections opened before the reload, one per name.
    let mut pool = Vec::new();
    for i in 0..16 {
        let sni = if i % 2 == 0 {
            "api.example.com"
        } else {
            "www.example.com"
        };
        pool.push(tlsc::Client::connect(addr, sni, &verified).await);
    }
    for c in &pool {
        assert!(c.cert == "exact" || c.cert == "wild", "{}", c.cert);
    }
    let pool: Arc<Vec<tokio::sync::Mutex<tlsc::Client>>> =
        Arc::new(pool.into_iter().map(tokio::sync::Mutex::new).collect());

    let failures = Arc::new(Mutex::new(Vec::<String>::new()));
    let done = Arc::new(AtomicUsize::new(0));
    let handshakes = Arc::new(Mutex::new(Vec::<Handshake>::new()));
    let total = RATE * SECONDS;
    let start = tokio::time::Instant::now();
    let mut tasks = Vec::with_capacity(total as usize);
    // When SIGHUP was sent and when the log said the reload was applied.
    let mut hup = None;
    for i in 0..total {
        tokio::time::sleep_until(start + Duration::from_micros(i * 1_000_000 / RATE)).await;
        if hup.is_none() && i == total / 2 {
            // Rotate the exact name's certificate halfway through.
            write_config(&cfg, up, "rotated");
            let sent = Instant::now();
            proc.signal("HUP");
            let line = tokio::task::block_in_place(|| proc.wait_log("reloaded on SIGHUP"));
            assert!(line.contains("\"version\":2"), "{line}");
            hup = Some((sent, Instant::now()));
        }
        let (pool, failures, done, handshakes) = (
            Arc::clone(&pool),
            Arc::clone(&failures),
            Arc::clone(&done),
            Arc::clone(&handshakes),
        );
        let (verified, unverified) = (verified.clone(), unverified.clone());
        tasks.push(tokio::spawn(async move {
            let result = if i % 4 == 0 {
                let (sni, opts) = match (i / 4) % 3 {
                    0 => ("api.example.com", &verified),
                    1 => ("www.example.com", &verified),
                    _ => ("unknown.test", &unverified),
                };
                let started = Instant::now();
                let mut c = tlsc::Client::connect(addr, sni, opts).await;
                handshakes.lock().unwrap().push(Handshake {
                    sni,
                    started,
                    ended: Instant::now(),
                    cert: c.cert,
                });
                c.get("/new").await
            } else {
                let slot = &pool[(i as usize) % pool.len()];
                let mut c = slot.lock().await;
                c.get("/kept").await
            };
            match result {
                Ok((200, body)) if body == "https" => {
                    done.fetch_add(1, Ordering::Relaxed);
                }
                other => failures
                    .lock()
                    .unwrap()
                    .push(format!("request {i}: {other:?}")),
            }
        }));
    }
    let elapsed = start.elapsed();
    for t in tasks {
        t.await.unwrap();
    }
    let failures = failures.lock().unwrap().clone();
    assert!(
        failures.is_empty(),
        "{} failed: {failures:?}",
        failures.len()
    );
    assert_eq!(done.load(Ordering::Relaxed) as u64, total);
    // The schedule held: 4,000 requests in about 4 seconds.
    assert!(
        elapsed < Duration::from_millis(SECONDS * 1000 + 500),
        "the load took {elapsed:?}"
    );

    // A handshake over before SIGHUP got the old certificate, one begun
    // after the reload was applied the rotated one; one in between may
    // have either. The other names never change.
    let (sent, applied) = hup.unwrap();
    let handshakes = std::mem::take(&mut *handshakes.lock().unwrap());
    let (mut old, mut new) = (0, 0);
    for h in handshakes.iter() {
        let want: &[&str] = match h.sni {
            "www.example.com" => &["wild"],
            "unknown.test" => &["default"],
            _ if h.ended < sent => &["exact"],
            _ if h.started > applied => &["rotated"],
            _ => &["exact", "rotated"],
        };
        assert!(want.contains(&h.cert), "{} got {}", h.sni, h.cert);
        if h.ended < sent {
            old += 1;
        } else if h.started > applied {
            new += 1;
        }
    }
    assert!(old > 100 && new > 100, "{old} before, {new} after");
    // The keep-alive connections kept their certificates and are open.
    for slot in pool.iter() {
        let c = slot.lock().await;
        assert!(c.cert == "exact" || c.cert == "wild");
        assert!(c.is_open());
    }
    eprintln!(
        "{total} requests at {RATE}/s in {elapsed:?}, {} on new connections ({old} before SIGHUP, {new} after the reload), reload applied {:?} after SIGHUP, 0 failed",
        handshakes.len(),
        applied - sent
    );
    drop(proc);
    let _ = std::fs::remove_dir_all(&dir);
}
