//! Golden transcripts of the command line, shared by the edgefront binary's
//! tests (crates/edgefront/tests/golden.rs) and the reference cachet
//! binary's tests (tests/golden_cli.rs). Both binaries run the same cases
//! and must print the same transcript, byte for byte, apart from their name.
//!
//! A transcript lists each invocation, its exit code, stdout and stderr. The
//! golden files spell the binary's name `{program}`, the package version
//! `{version}` and the scratch directory `{dir}`. JSON log lines are kept
//! as printed except for the values that change on every run: the timestamp,
//! request ids, elapsed times and loopback ports. Set `GOLDEN_BLESS=1` to
//! rewrite the files from the edgefront binary after a deliberate change.

#![allow(dead_code)]

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// A binary under test: its path and the name it calls itself.
pub struct Bin {
    pub path: &'static str,
    pub program: &'static str,
    /// The directory that holds the `.golden` files.
    pub golden: &'static str,
}

/// The node config used by every case. Health checks are spaced far apart so
/// no "origin marked down" line races the rest of the log.
fn node_config(listen: &str, extra: &str) -> String {
    format!(
        "node_id = \"edge-a\"\nlisten = \"{listen}\"\nadmin_listen = \"127.0.0.1:0\"\n\
         [[origins]]\nname = \"primary\"\nurl = \"http://127.0.0.1:9\"\n\
         [health]\ninterval_ms = 600000\n{extra}"
    )
}

const PEERS: &str = "[[peers]]\nname = \"edge-b\"\nurl = \"http://127.0.0.1:1\"\n";

/// The environment variables the binaries read; each invocation starts with
/// all of them removed so the caller's environment cannot leak in.
const ENV: [&str; 3] = ["CACHET_LOG", "CACHET_ADMIN_TOKEN", "CACHET_CLUSTER_SECRET"];

/// A scratch directory removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(bin: &Bin, case: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "golden-{}-{}-{case}",
            bin.program,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
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

/// Builds the transcript of one case.
struct Transcript {
    bin: &'static Bin,
    dir: Scratch,
    text: String,
}

impl Transcript {
    fn new(bin: &'static Bin, case: &str) -> Transcript {
        Transcript {
            bin,
            dir: Scratch::new(bin, case),
            text: String::new(),
        }
    }

    fn command(&self, env: &[(&str, &str)], args: &[&str]) -> Command {
        let mut cmd = Command::new(self.bin.path);
        cmd.args(args);
        for name in ENV {
            cmd.env_remove(name);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd
    }

    /// Records the command line with secrets shown by name only.
    fn header(&mut self, env: &[(&str, &str)], args: &[&str]) {
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push('$');
        for (k, v) in env {
            if k.ends_with("_LOG") {
                let _ = write!(self.text, " {k}={v}");
            } else {
                let _ = write!(self.text, " {k}=<{} bytes>", v.len());
            }
        }
        self.text.push_str(" {program}");
        for a in args {
            self.text.push(' ');
            self.text.push_str(a);
        }
        self.text.push('\n');
    }

    fn footer(&mut self, code: Option<i32>, stdout: &[String], stderr: &str) {
        match code {
            Some(code) => writeln!(self.text, "exit: {code}"),
            None => writeln!(self.text, "exit: killed by a signal"),
        }
        .unwrap();
        self.text.push_str("stdout:\n");
        for line in stdout {
            self.text.push_str(&normalize_log(line));
            self.text.push('\n');
        }
        self.text.push_str("stderr:\n");
        self.text.push_str(stderr);
        if !stderr.is_empty() && !stderr.ends_with('\n') {
            self.text.push('\n');
        }
    }

    /// Runs a command that exits on its own and records it.
    fn run(&mut self, env: &[(&str, &str)], args: &[&str]) {
        let out = self.command(env, args).output().unwrap();
        self.header(env, args);
        let stdout = String::from_utf8(out.stdout).unwrap();
        let lines: Vec<String> = stdout.lines().map(str::to_owned).collect();
        assert!(stdout.is_empty() || stdout.ends_with('\n'), "{stdout:?}");
        self.footer(
            out.status.code(),
            &lines,
            &String::from_utf8(out.stderr).unwrap(),
        );
    }

    fn finish(self) -> String {
        let dir = self.dir.0.to_str().unwrap().to_owned();
        self.text
            .replace(&dir, "{dir}")
            .replace(env!("CARGO_PKG_VERSION"), "{version}")
    }
}

/// Blanks the value that follows `"key":` in a JSON line, whether it is a
/// string or a number.
fn blank(line: &str, key: &str, with: &str) -> String {
    let pattern = format!("\"{key}\":");
    let Some(at) = line.find(&pattern) else {
        return line.to_owned();
    };
    let start = at + pattern.len();
    let rest = &line[start..];
    let end = if let Some(s) = rest.strip_prefix('"') {
        1 + s.find('"').expect("a closed string") + 1
    } else {
        rest.find([',', '}']).expect("a number")
    };
    format!("{}\"{with}\"{}", &line[..start], &rest[end..])
}

/// Replaces every loopback port with `<port>`.
fn blank_ports(line: &str) -> String {
    const HOST: &str = "127.0.0.1:";
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find(HOST) {
        out.push_str(&rest[..at + HOST.len()]);
        rest = &rest[at + HOST.len()..];
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 {
            out.push_str("<port>");
        }
        rest = &rest[digits..];
    }
    out.push_str(rest);
    out
}

fn normalize_log(line: &str) -> String {
    if !line.starts_with('{') {
        return line.to_owned();
    }
    let mut line = blank(line, "timestamp", "<time>");
    for (key, with) in [("request_id", "<id>"), ("elapsed_us", "<us>")] {
        line = blank(&line, key, with);
    }
    blank_ports(&line)
}

/// A running node whose stdout lines arrive on a channel.
struct Running {
    child: Child,
    lines: mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl Running {
    fn start(cmd: &mut Command) -> Running {
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Running {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// Waits for a log line with `message` and returns it.
    fn wait_for(&mut self, message: &str) -> String {
        let needle = format!("\"message\":\"{message}\"");
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("no {message:?} line; saw {:?}", self.seen));
            self.seen.push(line.clone());
            if line.contains(&needle) {
                return line;
            }
        }
    }

    fn terminate(&mut self) {
        let killed = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
    }

    /// Waits for the exit and returns its code, every stdout line and stderr.
    fn finish(mut self) -> (Option<i32>, Vec<String>, String) {
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        let status = self.child.wait().unwrap();
        self.seen.extend(self.lines.iter());
        (status.code(), std::mem::take(&mut self.seen), stderr)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads the string value of `"key":"..."` from a JSON line.
fn field<'a>(line: &'a str, key: &str) -> &'a str {
    let pattern = format!("\"{key}\":\"");
    let start = line.find(&pattern).expect(key) + pattern.len();
    let len = line[start..].find('"').unwrap();
    &line[start..start + len]
}

/// Sends one HTTP/1.1 request and returns the status line and the headers
/// named in `keep`, in that order.
fn exchange(addr: SocketAddr, head: &str, body: &str, keep: &[&str]) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "{head}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    s.read_to_string(&mut response).unwrap();
    let (head, rest) = response.split_once("\r\n\r\n").unwrap();
    let mut lines = head.split("\r\n");
    let mut out = format!("< {}\n", lines.next().unwrap());
    let headers: Vec<(String, &str)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim()))
        .collect();
    for name in keep {
        for (k, v) in &headers {
            if k == name {
                let _ = writeln!(out, "< {k}: {v}");
            }
        }
    }
    if !rest.is_empty() && !headers.iter().any(|(k, _)| k == "transfer-encoding") {
        let _ = writeln!(out, "< {rest}");
    }
    out
}

fn usage(t: &mut Transcript) {
    for args in [
        &["--version"][..],
        &[],
        &["--config"],
        &["--check-config"],
        &["--bogus", "x"],
        &["--version", "x"],
        &["--config", "a", "b"],
        &["--help"],
    ] {
        t.run(&[], args);
    }
}

fn check_config(t: &mut Transcript) {
    let good = t.dir.write("good.toml", &node_config("127.0.0.1:0", ""));
    let unknown = t.dir.write("unknown.toml", "node_id = \"x\"\nbogus = 1\n");
    let syntax = t.dir.write("syntax.toml", "node_id = \"edge-a\n");
    let invalid = t
        .dir
        .write("invalid.toml", &node_config("not an address", ""));
    let peers = t
        .dir
        .write("peers.toml", &node_config("127.0.0.1:0", PEERS));
    let missing = format!("{}/missing.toml", t.dir.0.to_str().unwrap());
    t.run(&[], &["--check-config", &good]);
    t.run(
        &[("CACHET_ADMIN_TOKEN", "token")],
        &["--check-config", &good],
    );
    t.run(&[], &["--check-config", &unknown]);
    t.run(&[], &["--check-config", &syntax]);
    t.run(&[], &["--check-config", &invalid]);
    t.run(&[], &["--check-config", &missing]);
    t.run(&[], &["--config", &missing]);
    t.run(&[], &["--check-config", &peers]);
    t.run(
        &[("CACHET_CLUSTER_SECRET", "short")],
        &["--check-config", &peers],
    );
    t.run(
        &[("CACHET_CLUSTER_SECRET", "a-cluster-secret-of-32-bytes!!!!")],
        &["--check-config", &peers],
    );
    t.run(&[], &["--config", &invalid]);
}

/// Serves one proxied request and one purge, then stops on SIGTERM.
fn serve(t: &mut Transcript) {
    let path = t.dir.write("node.toml", &node_config("127.0.0.1:0", ""));
    let env = [("CACHET_LOG", "info"), ("CACHET_ADMIN_TOKEN", "token")];
    let args = ["--config", path.as_str()];
    t.header(&env, &args);
    let mut node = Running::start(&mut t.command(&env, &args));
    let started = node.wait_for("cachet started");
    let proxy: SocketAddr = field(&started, "listen").parse().unwrap();
    let admin: SocketAddr = field(&started, "admin").parse().unwrap();

    let head = "GET /a HTTP/1.1\r\nhost: example.com";
    t.text
        .push_str(&format!("> {}\n", head.replace("\r\n", " | ")));
    let reply = exchange(proxy, head, "", &["cache-status", "content-type"]);
    t.text.push_str(&reply);
    node.wait_for("request");

    let head = "POST /purge HTTP/1.1\r\nhost: admin\r\nauthorization: Bearer token";
    let body = r#"{"urls":["http://example.com/a"]}"#;
    t.text.push_str("> POST /purge (bearer token)\n");
    t.text
        .push_str(&exchange(admin, head, body, &["content-type"]));
    node.wait_for("purge");

    node.terminate();
    let (code, lines, stderr) = node.finish();
    t.footer(code, &lines, &stderr);
}

/// `CACHET_LOG=warn` silences the info lines of a clean run.
fn quiet(t: &mut Transcript) {
    let path = t.dir.write("node.toml", &node_config("127.0.0.1:0", ""));
    let env = [("CACHET_LOG", "warn")];
    let args = ["--config", path.as_str()];
    t.header(&env, &args);
    let mut node = Running::start(&mut t.command(&env, &args));
    // Nothing is logged to say the node is up, so find its two listeners in
    // /proc and make a request to each before stopping it, as the serve case
    // does after its start line.
    for port in listening_ports(node.child.id()) {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let _ = exchange(addr, "GET /healthz HTTP/1.1\r\nhost: example.com", "", &[]);
    }
    node.terminate();
    let (code, lines, stderr) = node.finish();
    t.footer(code, &lines, &stderr);
}

/// Waits until process `pid` has bound its two listeners and returns their
/// ports, read from /proc/<pid>/fd and /proc/net/tcp.
fn listening_ports(pid: u32) -> Vec<u16> {
    let fd_dir = PathBuf::from(format!("/proc/{pid}/fd"));
    for _ in 0..500 {
        let ports = listening(&fd_dir);
        if ports.len() >= 2 {
            return ports;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the node never bound its listeners");
}

fn listening(fd_dir: &Path) -> Vec<u16> {
    let Ok(entries) = std::fs::read_dir(fd_dir) else {
        return Vec::new();
    };
    let inodes: Vec<String> = entries
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter_map(|link| {
            let link = link.to_str()?;
            Some(link.strip_prefix("socket:[")?.strip_suffix(']')?.to_owned())
        })
        .collect();
    let table = std::fs::read_to_string("/proc/net/tcp").unwrap_or_default();
    table
        .lines()
        .skip(1)
        .filter_map(|row| {
            let cols: Vec<&str> = row.split_whitespace().collect();
            // Column 3 is the state (0A is LISTEN) and column 9 the inode.
            if cols.get(3) != Some(&"0A")
                || !inodes.iter().any(|i| Some(&i.as_str()) == cols.get(9))
            {
                return None;
            }
            let (_, port) = cols.get(1)?.split_once(':')?;
            u16::from_str_radix(port, 16).ok()
        })
        .collect()
}

/// A listen address that is already taken: the node logs the error and
/// exits 1.
fn cannot_start(t: &mut Transcript) {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let path = t.dir.write("node.toml", &node_config(&addr, ""));
    t.run(&[], &["--config", &path]);
}

/// Runs `case` against `bin` and returns its normalized transcript.
pub fn transcript(bin: &'static Bin, case: &str) -> String {
    let mut t = Transcript::new(bin, case);
    match case {
        "usage" => usage(&mut t),
        "check_config" => check_config(&mut t),
        "serve" => serve(&mut t),
        "quiet" => quiet(&mut t),
        "cannot_start" => cannot_start(&mut t),
        _ => panic!("no case {case}"),
    }
    t.finish()
}

/// The golden file for `case`, with `{program}` spelled out.
fn expected(bin: &Bin, case: &str) -> String {
    let path = Path::new(bin.golden).join(format!("{case}.golden"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
        .replace("{program}", bin.program)
}

/// Runs `case` against `bin` and fails with a line diff if the transcript
/// differs from the golden file.
///
/// With `GOLDEN_BLESS=1`, the edgefront binary rewrites the golden file
/// instead; the cachet binary is then still checked against the new file.
pub fn check(bin: &'static Bin, case: &str) {
    let got = transcript(bin, case);
    if bin.program == "edgefront" && std::env::var_os("GOLDEN_BLESS").is_some() {
        let path = Path::new(bin.golden).join(format!("{case}.golden"));
        std::fs::write(path, got.replace("edgefront", "{program}")).unwrap();
        return;
    }
    let want = expected(bin, case);
    let got = got.replace("{program}", bin.program);
    if got != want {
        panic!(
            "{} differs from {case}.golden:\n{}",
            bin.program,
            diff(&want, &got)
        );
    }
}

/// A line diff: `-` lines are expected, `+` lines are what the binary did.
fn diff(want: &str, got: &str) -> String {
    let want: Vec<&str> = want.lines().collect();
    let got: Vec<&str> = got.lines().collect();
    let mut out = String::new();
    for i in 0..want.len().max(got.len()) {
        match (want.get(i), got.get(i)) {
            (Some(w), Some(g)) if w == g => {}
            (w, g) => {
                if let Some(w) = w {
                    let _ = writeln!(out, "{:>4} - {w}", i + 1);
                }
                if let Some(g) = g {
                    let _ = writeln!(out, "{:>4} + {g}", i + 1);
                }
            }
        }
    }
    out
}
