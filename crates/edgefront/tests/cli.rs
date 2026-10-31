//! The edgefront binary keeps cachet's command line: the same flags, exit
//! codes and messages under its own name, and it serves a node.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_edgefront");

fn edgefront(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env_remove("CACHET_ADMIN_TOKEN")
        .env_remove("CACHET_CLUSTER_SECRET")
        .output()
        .unwrap()
}

/// A scratch directory removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("edgefront-cli-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn write(&self, name: &str, text: &str) -> String {
        let path = self.0.join(name);
        std::fs::write(&path, text).unwrap();
        path.to_str().unwrap().to_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const CONFIG: &str = "node_id = \"edge-a\"\nlisten = \"127.0.0.1:0\"\nadmin_listen = \"127.0.0.1:0\"\n\
                      [[origins]]\nname = \"primary\"\nurl = \"http://127.0.0.1:9\"\n";

#[test]
fn version_names_the_binary() {
    let out = edgefront(&["--version"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("edgefront {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn bad_usage_exits_2_with_the_usage_text() {
    let usage = "usage: edgefront --config <file>\n       edgefront --check-config <file>\n       \
                 edgefront --version\n";
    for args in [
        &[][..],
        &["--config"],
        &["--bogus", "x"],
        &["--version", "x"],
    ] {
        let out = edgefront(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert_eq!(String::from_utf8(out.stderr).unwrap(), usage, "{args:?}");
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn check_config_reports_ok_or_the_error() {
    let dir = Scratch::new("check");
    let good = dir.write("good.toml", CONFIG);
    let out = edgefront(&["--check-config", &good]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{good}: ok\n")
    );

    let bad = dir.write("bad.toml", "node_id = \"x\"\nbogus = 1\n");
    let out = edgefront(&["--check-config", &bad]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.starts_with(&format!("edgefront: {bad}: ")), "{err}");
    assert!(err.contains("bogus"), "{err}");

    let missing = dir.0.join("missing.toml");
    let out = edgefront(&["--config", missing.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.starts_with("edgefront: cannot read "), "{err}");
}

#[test]
fn missing_secrets_are_a_config_error() {
    let dir = Scratch::new("secrets");
    let extra = "[[peers]]\nname = \"edge-b\"\nurl = \"http://127.0.0.1:1\"\n";
    let path = dir.write("peers.toml", &format!("{CONFIG}{extra}"));
    let out = edgefront(&["--check-config", &path]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.starts_with("edgefront: "), "{err}");
    assert!(err.contains("CACHET_CLUSTER_SECRET"), "{err}");
}

/// Reads the bound proxy address from the JSON "cachet started" log line.
fn started_addr(line: &str) -> Option<SocketAddr> {
    if !line.contains("\"cachet started\"") {
        return None;
    }
    let rest = &line[line.find("\"listen\":\"")? + 10..];
    rest[..rest.find('"')?].parse().ok()
}

#[cfg(unix)]
#[test]
fn serves_requests_and_stops_cleanly_on_sigterm() {
    let dir = Scratch::new("serve");
    let path = dir.write("edgefront.toml", CONFIG);
    let mut child = Command::new(BIN)
        .args(["--config", &path])
        .env_remove("CACHET_ADMIN_TOKEN")
        .env_remove("CACHET_CLUSTER_SECRET")
        .env("CACHET_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let addr = loop {
        let line = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the node logs its start");
        if let Some(addr) = started_addr(&line) {
            break addr;
        }
    };

    // The origin is down, so the proxy answers 502 itself.
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(b"GET /a HTTP/1.1\r\nhost: example.com\r\nconnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    s.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 502 "), "{response}");
    assert!(
        response.to_ascii_lowercase().contains("\r\ncache-status: "),
        "{response}"
    );

    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
    let logged: Vec<String> = rx.iter().collect();
    assert!(
        logged.iter().any(|l| l.contains("\"shutting down\"")),
        "{logged:?}"
    );
}
