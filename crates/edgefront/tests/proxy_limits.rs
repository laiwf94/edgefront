//! `edgefront proxy --limits`: every section of a limits file checked by
//! `--check-config` and refused with its field when wrong, and a proxy
//! run with all of them at once: the rules, a connection quota, overload
//! mode, listener caps and a recording, a mode change applied in place and
//! a change that needs a restart refused, and the recording flushed when
//! SIGTERM stops it. Also `edgefront limits`' ban endpoints, refusals
//! included.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_edgefront");

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pl-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An upstream answering every request with `200 ok`.
fn upstream() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    thread::spawn(move || {
        for c in l.incoming() {
            let Ok(c) = c else { return };
            thread::spawn(move || {
                let mut w = c.try_clone().unwrap();
                let mut r = BufReader::new(c);
                loop {
                    let mut line = String::new();
                    loop {
                        line.clear();
                        if r.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    if w.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    addr
}

fn proxy_json(dir: &Path, up: &str) -> PathBuf {
    let p = dir.join("proxy.json");
    std::fs::write(
        &p,
        format!(
            "{{\"listen\":\"127.0.0.1:0\",\"admin_listen\":\"127.0.0.1:0\",\
             \"routes\":[{{\"host\":\"*\",\"path_prefix\":\"/\",\"cluster\":\"c\"}}],\
             \"clusters\":[{{\"name\":\"c\",\"endpoints\":[{{\"address\":\"{up}\"}}]}}]}}"
        ),
    )
    .unwrap();
    p
}

fn check(config: &Path, limits: &Path) -> (i32, String) {
    let out = Command::new(BIN)
        .args(["proxy", "--check-config"])
        .arg(config)
        .arg("--limits")
        .arg(limits)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
    )
}

const RULE: &str = "[[http_limits]]\nname = \"api\"\nrate = \"2/min burst 2\"\nkey = [\"header:x-k\"]\nmode = \"shadow\"\n";

#[test]
fn check_config_reads_every_section_and_names_each_mistake() {
    let dir = scratch("check");
    let config = proxy_json(&dir, "127.0.0.1:9");
    let limits = dir.join("limits.toml");
    std::fs::write(
        &limits,
        format!(
            "{RULE}[connection_quota]\nmax_requests = 5\n[overload]\ncpu = [50, 90]\n\
             [recording]\ndir = \"rec\"\n[listener]\nmax_in_flight = 10\n"
        ),
    )
    .unwrap();
    let (code, out) = check(&config, &limits);
    assert_eq!(code, 0, "{out}");
    for (body, want) in [
        (
            "[[http_limits]]\nname = \"x\"\nrate = \"1/s\"\nkey = [\"ip\"]\n",
            "unknown key selector",
        ),
        (
            "[[http_limits]]\nname = \"x\"\nrate = \"1/s\"\nroutes = [5]\n",
            "route 5 does not exist",
        ),
        (
            "[connection_quota]\nmax_requests = 0\n",
            "connection_quota.max_requests",
        ),
        ("[overload]\ncpu = [90, 10]\n", "overload.cpu"),
        ("[recording]\ndir = \"r\"\nring = 0\n", "recording: ring"),
        ("[listener]\nconn_rate = \"x\"\n", "listener.conn_rate"),
        ("[listener\n", "TOML parse error"),
    ] {
        std::fs::write(&limits, body).unwrap();
        let (code, out) = check(&config, &limits);
        assert_eq!(code, 2, "{body}: {out}");
        assert!(out.contains(want), "{body}: {out}");
    }
    let (code, out) = check(&config, &dir.join("missing.toml"));
    assert_eq!(code, 2);
    assert!(out.contains("cannot read"), "{out}");
    // Serving with a bad file is refused the same way.
    std::fs::write(&limits, "[overload]\ncpu = [90, 10]\n").unwrap();
    let out = Command::new(BIN)
        .args(["proxy", "--config"])
        .arg(&config)
        .arg("--limits")
        .arg(&limits)
        .env("EDGEFRONT_LOG", "error")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("overload.cpu"));
    let _ = std::fs::remove_dir_all(&dir);
}

struct Proxy {
    child: Child,
    lines: mpsc::Receiver<String>,
    addr: String,
}

impl Proxy {
    fn wait_log(&self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(l) if l.contains(needle) => return l,
                Ok(_) => {}
                Err(_) => panic!("no log line with {needle}"),
            }
        }
    }

    fn get(&self, key: &str) -> u16 {
        let mut c = TcpStream::connect(&self.addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            c,
            "GET / HTTP/1.1\r\nHost: t\r\nx-k: {key}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        let _ = c.read_to_string(&mut out);
        out.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_proxy_with_every_section_records_and_changes_modes_in_place() {
    let dir = scratch("run");
    let config = proxy_json(&dir, &upstream());
    let limits = dir.join("limits.toml");
    let full = format!(
        "{RULE}[connection_quota]\nmax_requests = 100\n[overload]\ncpu = [60, 85]\n\
         [recording]\ndir = \"rec\"\n[listener]\nrequest_rate = \"1000/s\"\nconn_rate = \"1000/s\"\nmax_in_flight = 100\n"
    );
    std::fs::write(&limits, &full).unwrap();
    let mut child = Command::new(BIN)
        .args(["proxy", "--config"])
        .arg(&config)
        .arg("--limits")
        .arg(&limits)
        .env("EDGEFRONT_LOG", "info")
        .env("EDGEFRONT_DRAIN_PERIOD", "0s")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = mpsc::channel();
    thread::spawn(move || {
        for l in BufReader::new(stdout).lines() {
            let Ok(l) = l else { break };
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    let mut p = Proxy {
        child,
        lines,
        addr: String::new(),
    };
    let listening = p.wait_log("\"listening\"");
    let at = listening.find("\"proxy\":\"").unwrap() + 9;
    p.addr = listening[at..].split('"').next().unwrap().to_owned();
    // Shadow: nothing refused.
    for _ in 0..4 {
        assert_eq!(p.get("a"), 200);
    }
    // A change to the rate needs a restart: refused and logged.
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(&limits, full.replace("2/min burst 2", "3/min burst 3")).unwrap();
    p.wait_log("http limits not applied");
    // A mode change is applied in place, with the counts it had.
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(
        &limits,
        full.replace("mode = \"shadow\"", "mode = \"enforce\""),
    )
    .unwrap();
    p.wait_log("http limit mode changed");
    assert_eq!(p.get("a"), 429, "spent while shadowed");
    assert_eq!(p.get("b"), 200);
    // SIGTERM: the recording is flushed on the way out.
    let ok = Command::new("kill")
        .args(["-TERM", &p.child.id().to_string()])
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = p.child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "the proxy did not stop");
        thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success());
    let rec = edgefront_limiter::record::read(&dir.join("rec")).unwrap();
    assert!(rec.records.len() >= 6, "{}", rec.records.len());
    assert_eq!(rec.rules, ["api"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `edgefront limits` with an admin socket, and one HTTP request on it.
fn admin(sock: &Path, method: &str, target: &str) -> (u16, String) {
    let mut s = std::os::unix::net::UnixStream::connect(sock).unwrap();
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out.get(9..12).and_then(|v| v.parse().ok()).unwrap_or(0);
    let body = out.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned();
    (status, body)
}

#[test]
fn the_ban_endpoints_check_what_they_are_given() {
    let dir = std::path::PathBuf::from(format!("/tmp/pb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("a.sock");
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let conf = dir.join("pacerd.conf");
    std::fs::write(
        &conf,
        format!("admin_socket {}\nlistener l {{\n  listen 127.0.0.1:{port}\n  upstream 127.0.0.1:9\n}}\n", sock.display()),
    )
    .unwrap();
    // A bans file that does not read stops the start.
    let bad = dir.join("bad-bans.txt");
    std::fs::write(&bad, "nonsense\n").unwrap();
    let out = Command::new(BIN)
        .args(["limits", "-c"])
        .arg(&conf)
        .env("EDGEFRONT_BANS_FILE", &bad)
        .output()
        .unwrap();
    assert_ne!(out.status.code(), Some(0));
    // A bans file in a directory that does not exist cannot be saved; the
    // ban still holds and the failure is logged.
    let bans = dir.join("missing-dir/bans.txt");
    let log = dir.join("log");
    let mut child = Command::new(BIN)
        .args(["limits", "-c"])
        .arg(&conf)
        .env("EDGEFRONT_BANS_FILE", &bans)
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(&log)
        .unwrap_or_default()
        .contains("msg=ready")
    {
        assert!(Instant::now() < deadline, "no ready line");
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(admin(&sock, "POST", "/ban?addr=nope&for=5").0, 400);
    assert_eq!(admin(&sock, "POST", "/ban?addr=10.0.0.1&for=0").0, 400);
    assert_eq!(
        admin(&sock, "POST", "/ban?addr=10.0.0.1&for=5&reason=two%20words").0,
        400
    );
    assert_eq!(admin(&sock, "GET", "/ban").0, 405);
    assert_eq!(
        admin(&sock, "POST", "/ban?addr=10.0.0.1&for=60"),
        (200, "ok banned\n".into())
    );
    assert_eq!(
        admin(&sock, "POST", "/ban?addr=2001:db8::1&for=60&reason=abuse").0,
        200
    );
    assert_eq!(
        admin(&sock, "POST", "/ban?addr=::ffff:10.0.0.2&for=60").0,
        200
    );
    let (code, list) = admin(&sock, "GET", "/bans");
    assert_eq!(code, 200);
    assert!(list.contains("10.0.0.1 60 manual\n"), "{list}");
    assert!(list.contains("2001:db8::1 60 abuse\n"), "{list}");
    assert!(
        list.contains("10.0.0.2 60 manual\n"),
        "an IPv4-mapped address bans the IPv4 one: {list}"
    );
    assert_eq!(admin(&sock, "POST", "/unban").0, 400);
    assert_eq!(admin(&sock, "POST", "/unban?addr=10.0.0.9").0, 404);
    assert_eq!(
        admin(&sock, "POST", "/unban?addr=10.0.0.1"),
        (200, "ok unbanned\n".into())
    );
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .contains("bans not saved")
    );
    let _ = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
