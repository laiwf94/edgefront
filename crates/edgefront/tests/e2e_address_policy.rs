//! Address policy end to end: `edgefront limits` behind two trusted
//! proxies (the load balancer the test connects from, 127.0.0.1, and an
//! edge proxy, 192.0.2.7, that appends its hop to X-Forwarded-For),
//! with a deny class read from a list file, an allow class and a limited
//! partner class. Each client is named only by the header, so every
//! limit below is applied to the client behind the proxies, never to the
//! one peer address all connections come from.
//!
//! It checks: deny refuses with nothing read and says so in the log; a
//! client that forges entries left of the real hop gains nothing; allow
//! lifts the listener's per-client cap; a partner gets its class's cap, a
//! second partner its own; an unlisted client gets the listener's own
//! limits, keyed by that client; the stats line counts every verdict;
//! `edgefront check` and `limits -t` report the lists' sizes; a reload
//! with a longer deny list applies to new connections and keeps the
//! running ones; and SIGTERM still drains to exit 0.

use std::net::{SocketAddr, TcpListener as StdListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const BIN: &str = env!("CARGO_BIN_EXE_edgefront");

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch() -> Scratch {
    static N: AtomicUsize = AtomicUsize::new(0);
    let p = std::env::temp_dir().join(format!(
        "efap-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&p).unwrap();
    Scratch(p)
}

/// macOS caps Unix socket paths at 104 bytes, so the admin socket lives
/// in /tmp.
fn socket_path() -> PathBuf {
    PathBuf::from(format!("/tmp/efap-{}.sock", std::process::id()))
}

fn free_port() -> u16 {
    StdListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Daemon {
    child: Child,
    log: PathBuf,
}

impl Daemon {
    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn wait_log(&mut self, needle: &str, count: usize) -> bool {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if self.log_text().matches(needle).count() >= count {
                return true;
            }
            if let Ok(Some(_)) = self.child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.log_text().matches(needle).count() >= count
    }

    fn signal(&self, sig: &str) {
        let ok = Command::new("kill")
            .args([format!("-{sig}"), self.child.id().to_string()])
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn echo_upstream() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    a
}

/// A request from `client` as the edge proxy forwards it: whatever the
/// client itself sent in X-Forwarded-For (`forged`), then the client's
/// address as the edge saw it, then the edge's own hop.
fn head(forged: &str, client: &str) -> String {
    format!(
        "GET / HTTP/1.1\r\nHost: shop.test\r\nX-Forwarded-For: {forged}{client}, 192.0.2.7\r\n\r\n"
    )
}

/// Opens a connection for `client` and reports whether its request came
/// back through the echo upstream (admitted) or the connection was closed
/// with nothing sent (refused). An admitted stream is returned open.
async fn open(addr: SocketAddr, forged: &str, client: &str) -> Option<TcpStream> {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let h = head(forged, client);
    s.write_all(h.as_bytes()).await.unwrap();
    let mut back = vec![0u8; h.len()];
    match tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut back)).await {
        Ok(Ok(_)) if back == h.as_bytes() => Some(s),
        Ok(_) => None,
        Err(_) => panic!("{client}: neither echoed nor closed in 5 s"),
    }
}

fn ctl(sock: &Path, cmd: &str) -> String {
    let out = Command::new(BIN)
        .args(["ctl", "limits", "-s", sock.to_str().unwrap(), cmd])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allow_deny_and_class_limits_behind_two_trusted_proxies() {
    let dir = scratch();
    let upstream = echo_upstream().await;
    let port = free_port();
    let sock = socket_path();
    let _ = std::fs::remove_file(&sock);
    std::fs::write(dir.0.join("blocked.txt"), "# abusers\n198.51.100.0/24\n").unwrap();
    let cfg = dir.0.join("edge.toml");
    std::fs::write(
        &cfg,
        format!(
            r#"workers = 1
admin_socket = "{sock}"
drain_timeout = "5s"

[[listeners]]
name = "web"
listen = "127.0.0.1:{port}"
upstream = "{upstream}"
per_ip_max_conns = 1

[address_policy]
header = "x-forwarded-for"
trusted_proxies = ["127.0.0.1", "192.0.2.7"]

[[address_policy.classes]]
name = "blocked"
action = "deny"
files = ["blocked.txt"]

[[address_policy.classes]]
name = "office"
action = "allow"
prefixes = ["203.0.113.10"]

[[address_policy.classes]]
name = "partners"
action = "limit"
prefixes = ["203.0.113.0/24"]
per_ip_max_conns = 2
per_ip_conn_rate = "100/s burst 100"
"#,
            sock = sock.display()
        ),
    )
    .unwrap();

    // The config checks, and its lists are reported with their sizes.
    let out = Command::new(BIN)
        .args(["check", cfg.to_str().unwrap()])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("address_policy: 2 trusted proxies, 3 classes, 3 prefixes, "),
        "{text}"
    );
    assert!(
        text.contains("  class blocked: deny, 1 prefixes\n"),
        "{text}"
    );
    assert!(text.contains("blocked.txt: 1 prefixes\n"), "{text}");
    let out = Command::new(BIN)
        .args(["limits", "-c", cfg.to_str().unwrap(), "-t"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains("3 classes, 3 prefixes")
    );

    let log = dir.0.join("log");
    let mut d = Daemon {
        child: Command::new(BIN)
            .args(["limits", "-c", cfg.to_str().unwrap()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
        log,
    };
    assert!(d.wait_log("msg=ready", 1), "{}", d.log_text());
    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    // Deny, from the list file, with nothing read.
    assert!(open(addr, "", "198.51.100.9").await.is_none());
    assert!(
        d.wait_log("reason=class_deny class=blocked client=198.51.100.9", 1),
        "{}",
        d.log_text()
    );
    // Forging entries left of the edge's hop changes nothing: the forged
    // office address and a forged trusted proxy are never reached.
    assert!(open(addr, "203.0.113.10, ", "198.51.100.9").await.is_none());
    assert!(
        open(addr, "203.0.113.10, 192.0.2.7, ", "198.51.100.9")
            .await
            .is_none()
    );
    assert!(d.wait_log("reason=class_deny class=blocked client=198.51.100.9", 3));

    // Allow lifts the listener's cap of one connection per client.
    let mut office = Vec::new();
    for _ in 0..3 {
        office.push(
            open(addr, "", "203.0.113.10")
                .await
                .expect("office admitted"),
        );
    }

    // A partner has its class's cap of two; another partner its own.
    let p1 = open(addr, "", "203.0.113.5").await.expect("partner 1");
    let p2 = open(addr, "", "203.0.113.5").await.expect("partner 2");
    assert!(open(addr, "", "203.0.113.5").await.is_none());
    assert!(d.wait_log("reason=class_conns class=partners client=203.0.113.5", 1));
    let other_partner = open(addr, "", "203.0.113.6")
        .await
        .expect("another partner");
    // The forged prefix does not let a partner pose as the office.
    assert!(open(addr, "203.0.113.10, ", "203.0.113.5").await.is_none());
    drop(p1);
    // Its hold comes back once the connection closes.
    let mut back = None;
    for _ in 0..100 {
        back = open(addr, "", "203.0.113.5").await;
        if back.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        back.is_some(),
        "the closed partner connection's hold came back"
    );

    // An unlisted client gets the listener's own cap of one, keyed by the
    // client behind the proxies: a second client is not counted with it.
    let c1 = open(addr, "", "100.64.0.1").await.expect("default client");
    assert!(open(addr, "", "100.64.0.1").await.is_none());
    assert!(d.wait_log("reason=per_ip_conns", 1));
    let c2 = open(addr, "", "100.64.0.2")
        .await
        .expect("second default client");

    // The stats line counts each verdict.
    let stats = ctl(&sock, "stats");
    let line = stats
        .lines()
        .find(|l| l.starts_with("address_policy "))
        .unwrap_or_else(|| panic!("{stats}"));
    for want in [
        "classes=3",
        "prefixes=3",
        "trusted_proxies=2",
        "class_deny=3",
        "class_allow=3",
        "default=3",
        "malformed_hop=0",
    ] {
        assert!(line.split(' ').any(|f| f == want), "{want} in {line}");
    }
    // Two refusals at the cap, and any retries of the wait for the hold.
    let conns: u64 = line
        .split(' ')
        .find_map(|f| f.strip_prefix("class_conns="))
        .unwrap()
        .parse()
        .unwrap();
    assert!(conns >= 2, "{line}");

    // Reload with 100.64.0.0/24 added to the deny list: new connections
    // from it are refused, the open one keeps going.
    std::fs::write(
        dir.0.join("blocked.txt"),
        "198.51.100.0/24\n100.64.0.0/24\n",
    )
    .unwrap();
    d.signal("HUP");
    assert!(d.wait_log("msg=reloaded", 1), "{}", d.log_text());
    assert!(open(addr, "", "100.64.0.3").await.is_none());
    assert!(d.wait_log("reason=class_deny class=blocked client=100.64.0.3", 1));
    let mut c1 = c1;
    c1.write_all(b"still").await.unwrap();
    let mut b = [0u8; 5];
    c1.read_exact(&mut b).await.unwrap();
    assert_eq!(&b, b"still");
    let stats = ctl(&sock, "stats");
    assert!(
        stats.contains("address_policy classes=3 prefixes=4 "),
        "{stats}"
    );

    // A reload whose list file is broken is refused and the running
    // policy stays.
    std::fs::write(
        dir.0.join("blocked.txt"),
        "198.51.100.0/24\n100.64.0.1/24\n",
    )
    .unwrap();
    d.signal("HUP");
    assert!(d.wait_log("reload rejected", 1), "{}", d.log_text());
    assert!(
        d.log_text().contains("blocked.txt: line 2: 100.64.0.1/24"),
        "{}",
        d.log_text()
    );
    assert!(open(addr, "", "100.64.0.4").await.is_none());

    drop((office, p2, other_partner, back, c1, c2));
    d.signal("TERM");
    let end = Instant::now() + Duration::from_secs(20);
    let code = loop {
        if let Some(st) = d.child.try_wait().unwrap() {
            break st.code();
        }
        assert!(Instant::now() < end, "did not drain:\n{}", d.log_text());
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(code, Some(0), "{}", d.log_text());
    let _ = std::fs::remove_file(&sock);
}
