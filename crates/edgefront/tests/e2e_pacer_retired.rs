//! End to end: every feature pacer had works with legacy/pacer gone.
//!
//! pacer was two things, and each is checked here without any of its code:
//!
//! - **libpacer**, the algorithms: edgefront-limiter gives exactly the
//!   answers libpacer recorded before it was deleted, for its golden
//!   transcripts, a million generated operations and pacerd's config
//!   reader on its whole corpus and 2,000 mutants.
//! - **pacerd and pacerctl**, the proxy: one `edgefront limits` started
//!   from pacer's own example config (with its addresses moved to free
//!   ports) checks its config like `pacerd -t`, limits a client's
//!   connections and connection rate, shapes a download, reports pacerctl's
//!   counters and the clients behind them, reloads in place and drains on
//!   SIGTERM. The 23 integration scenarios, against pacerd's recorded
//!   transcripts, are tests/pacer_scripts.rs; the descriptor, table-full,
//!   30-second shaping and 10,000-connection drain tests are
//!   tests/limits.rs; the live demo is tests/e2e_l4_limits.rs.
//!
//! It also checks that nothing in the build still points at legacy/pacer,
//! and that the C-versus-Rust benchmark that closed the port is published.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use edgefront_difftest::{pacer, pacer_config};

const BIN: &str = env!("CARGO_BIN_EXE_edgefront");

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn libpacers_recorded_answers_still_hold() {
    let algo = pacer::check_recorded(&pacer::recorded_dir(), None).unwrap();
    assert!(algo.mismatches.is_empty(), "{:?}", algo.mismatches);
    assert_eq!((algo.goldens, algo.scripts), (4, 100));
    assert!(algo.lines >= 1_000_000, "{}", algo.lines);
    let cfg = pacer_config::check_recorded(&pacer_config::pacer_root()).unwrap();
    assert!(cfg.mismatches.is_empty(), "{:?}", cfg.mismatches);
    assert_eq!(cfg.mutants, 2000);
    assert!(cfg.inputs > 1_500, "{}", cfg.inputs);
}

#[test]
fn nothing_in_the_build_points_at_legacy_pacer() {
    let r = root();
    assert!(
        !r.join("legacy/pacer").exists(),
        "legacy/pacer is still there"
    );
    assert!(!r.join(".github/workflows/legacy-pacer.yml").exists());
    for file in [
        "Makefile",
        "Dockerfile",
        ".github/workflows/ci.yml",
        ".github/workflows/difftest.yml",
        "crates/edgefront-difftest/Cargo.toml",
    ] {
        let text = std::fs::read_to_string(r.join(file)).unwrap();
        for line in text.lines().filter(|l| l.contains("legacy/pacer")) {
            // Only comments may still name it, as history.
            assert!(line.trim_start().starts_with('#'), "{file}: {line}");
        }
    }
    assert!(!r.join("crates/edgefront-difftest/build.rs").exists());
}

#[test]
fn the_c_versus_rust_benchmark_is_published() {
    let doc = std::fs::read_to_string(root().join("docs/benchmarks/pacer-port.md")).unwrap();
    for want in [
        "| shared-gcra | Rust |",
        "| shared-gcra | C |",
        "| mutex-gcra | Rust |",
        "| table-spread | Rust |",
        "| table-hot | Rust |",
        "16 threads",
        "64-byte round trip, p99",
        "bulk download, one connection",
        "round trips/s, 32 connections",
        "bytes per client",
    ] {
        assert!(doc.contains(want), "{want:?} missing from pacer-port.md");
    }
    for data in ["decisions", "l4", "memory"] {
        let p = root().join(format!("docs/benchmarks/data/pacer-port-{data}.json"));
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        assert!(!v.is_null());
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// An echo upstream.
fn upstream() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 16384];
                loop {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(k) => {
                            if s.write_all(&buf[..k]).is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    a
}

fn works(addr: SocketAddr) -> Option<TcpStream> {
    let mut s = TcpStream::connect(addr).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    let mut b = [0u8; 4];
    (s.write_all(b"ping").is_ok() && s.read_exact(&mut b).is_ok() && &b == b"ping").then_some(s)
}

fn refused(addr: SocketAddr) -> bool {
    let Ok(mut s) = TcpStream::connect(addr) else {
        return true;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    let mut b = [0u8; 1];
    matches!(s.read(&mut b), Ok(0) | Err(_))
}

fn ctl(sock: &Path, args: &[&str]) -> String {
    let out = Command::new(BIN)
        .args(["ctl", "limits", "-s", sock.to_str().unwrap()])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

struct Daemon(Child, PathBuf);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn wait_log(log: &Path, needle: &str) -> bool {
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if std::fs::read_to_string(log).is_ok_and(|t| t.contains(needle)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn pacerds_example_runs_on_edgefront_with_every_feature() {
    // pacer's own example, with its addresses moved to free ports, its
    // admin socket to a short path, its per-client bandwidth lowered so an
    // echo shows the shaping, and its connection rate slowed to one an
    // hour (bursts of 21) so the rate's refusal does not race the refill.
    // Every connection comes from 127.0.0.1, one client: 20 held, the
    // 21st shaped, the 22nd refused by the rate until a reload.
    let example = std::fs::read_to_string(root().join("examples/limiter/pacerd.conf")).unwrap();
    let dir = std::env::temp_dir().join(format!("pacer-retired-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = PathBuf::from(format!("/tmp/pacer-retired-{}.sock", std::process::id()));
    let echo = upstream();
    let port = free_port();
    let mut conf = String::new();
    let mut in_listener = 0;
    for line in example.lines() {
        let t = line.trim_start();
        let replaced = if t.starts_with("admin_socket ") {
            format!("admin_socket {}", sock.display())
        } else if t.starts_with("listen ") {
            in_listener += 1;
            if in_listener == 1 {
                format!("    listen 127.0.0.1:{port}")
            } else {
                format!("    listen 127.0.0.1:{}", free_port())
            }
        } else if t.starts_with("upstream ") {
            format!("    upstream {echo}")
        } else if t.starts_with("per_ip_bandwidth ") {
            "    per_ip_bandwidth 256k/s burst 16k".to_string()
        } else if t.starts_with("per_ip_conn_rate ") {
            "    per_ip_conn_rate 1/h burst 21".to_string()
        } else {
            line.to_string()
        };
        conf.push_str(&replaced);
        conf.push('\n');
    }
    let path = dir.join("pacerd.conf");
    std::fs::write(&path, &conf).unwrap();
    // pacerd -t
    let out = Command::new(BIN)
        .args(["limits", "-c", path.to_str().unwrap(), "-t"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = dir.join("log");
    let child = Command::new(BIN)
        .args(["limits", "-c", path.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let mut d = Daemon(child, dir.clone());
    assert!(
        wait_log(&log, "msg=ready"),
        "{}",
        std::fs::read_to_string(&log).unwrap()
    );
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    assert_eq!(ctl(&sock, &["ping"]), "ok pong\n");

    // Connection limits: the example allows 20 open per client.
    let mut held: Vec<TcpStream> = (0..20).map_while(|_| works(addr)).collect();
    assert_eq!(held.len(), 20);
    assert!(refused(addr));
    assert!(wait_log(&log, "reason=per_ip_conns"));
    let stats = ctl(&sock, &["stats"]);
    assert!(stats.contains(" open=20 accepted=20 "), "{stats}");
    assert!(stats.contains(" rejected_per_ip_conns=1 "), "{stats}");
    let top = ctl(&sock, &["top", "1"]);
    assert!(
        top.contains("client=127.0.0.1 tracked=yes open=20 "),
        "{top}"
    );
    held.clear();
    let end = Instant::now() + Duration::from_secs(5);
    while !ctl(&sock, &["stats"]).contains(" open=0 ") {
        assert!(Instant::now() < end, "connections did not close");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Shaping: 256 KiB/s with a 16 KiB burst. The echo moves 136 KiB up
    // and 136 KiB back, both charged to the client: about 1 s.
    // The 21st connection, the last token of the burst.
    let mut s = works(addr).unwrap();
    let payload = vec![7u8; 136 * 1024];
    let t = Instant::now();
    let writer = {
        let mut w = s.try_clone().unwrap();
        let p = payload.clone();
        std::thread::spawn(move || w.write_all(&p).unwrap())
    };
    let mut got = vec![0u8; payload.len()];
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.read_exact(&mut got).unwrap();
    writer.join().unwrap();
    let took = t.elapsed();
    assert!(
        took >= Duration::from_millis(900) && took < Duration::from_secs(3),
        "{took:?}"
    );
    assert_eq!(got, payload);
    assert!(ctl(&sock, &["stats"]).contains(" throttled="));
    drop(s);

    // The burst is spent: the rate refuses.
    assert!(refused(addr));
    assert!(wait_log(&log, "reason=per_ip_rate"));

    // Reload in place: a larger burst applies at once, and the client
    // keeps its debt as time (as pacerd did), so the larger burst lets it
    // in again where a faster rate alone would not for about an hour.
    std::fs::write(
        &path,
        conf.replace(
            "per_ip_conn_rate 1/h burst 21",
            "per_ip_conn_rate 1/h burst 30",
        ),
    )
    .unwrap();
    assert_eq!(ctl(&sock, &["reload"]), "ok reloaded\n");
    assert!(works(addr).is_some());

    // Drain on SIGTERM: refused at once, exit 0, drained.
    assert!(
        Command::new("kill")
            .arg(d.0.id().to_string())
            .status()
            .unwrap()
            .success()
    );
    let end = Instant::now() + Duration::from_secs(40);
    let code = loop {
        if let Some(st) = d.0.try_wait().unwrap() {
            break st.code();
        }
        assert!(Instant::now() < end, "did not drain");
        std::thread::sleep(Duration::from_millis(20));
    };
    let text = std::fs::read_to_string(&log).unwrap();
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("msg=draining reason=SIGTERM"));
    assert!(text.contains("msg=drained"));
    assert!(TcpStream::connect(addr).is_err());
    let _ = std::fs::remove_file(&sock);
}
