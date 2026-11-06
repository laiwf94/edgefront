//! The live demo (examples/demo.sh) as a test, shared by the edgefront
//! binary (crates/edgefront/tests/e2e_workspace.rs) and the reference
//! cachet binary (tests/e2e_workspace.rs at the root).
//!
//! Two nodes, edge-a and edge-b, run as child processes in front of an
//! origin that this file serves on a plain TCP listener. The five steps are
//! the demo's: a stored miss then a hit, revalidation with an entity tag,
//! request collapsing, stale-if-error with the origin down, and a purge
//! that reaches the peer. Every response's status, `Cache-Status` header
//! and body is recorded, with the origin's request counts and the purge
//! reply, and the transcript must equal `tests/fixtures/e2e_workspace.txt`
//! byte for byte. Nothing in it is blanked.
//!
//! What would otherwise vary between runs is pinned down instead:
//! - The origin sends no `Date` header, so a stored object's age starts at
//!   the origin's response delay, well under a second, and `ttl=` is the
//!   full lifetime for every request made within that second.
//! - Where the demo sleeps until an object goes stale, the origin sends
//!   `Age: 10` with `max-age=2`, so the object is stale on arrival and
//!   reports `ttl=-8`.
//! - The origin holds `/slow` until all 50 clients have sent their request,
//!   so exactly one leads and 49 are collapsed onto it.
//! - Health checks are spaced far apart, so the origin is never marked down
//!   behind the test's back and an outage shows up as a failed connect.
//!
//! Set `E2E_BLESS=1` to rewrite the fixture from the edgefront binary after
//! a deliberate change; the cachet binary is always checked.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// A binary under test and the fixture it must reproduce.
pub struct Bin {
    pub path: &'static str,
    pub program: &'static str,
    pub fixture: &'static str,
}

const TOKEN: &str = "demo-admin-token";
const CLUSTER_SECRET: &str = "demo-cluster-secret-at-least-32-bytes";
const TIMEOUT: Duration = Duration::from_secs(10);
const CLIENTS: usize = 50;

// ---------------------------------------------------------------------------
// The origin: examples/origin.rs's routes, answered one request per
// connection so a stopped origin refuses the next connect.

#[derive(Default)]
struct OriginState {
    version: AtomicU64,
    counts: Mutex<BTreeMap<String, u64>>,
    /// Set once every `/slow` client has sent its request.
    slow_open: Mutex<bool>,
    slow_cv: Condvar,
}

impl OriginState {
    fn count(&self, path: &str) -> u64 {
        self.counts.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    fn open_slow(&self) {
        *self.slow_open.lock().unwrap() = true;
        self.slow_cv.notify_all();
    }

    fn wait_slow(&self) {
        let open = self.slow_open.lock().unwrap();
        let (_open, timeout) = self
            .slow_cv
            .wait_timeout_while(open, TIMEOUT, |open| !*open)
            .unwrap();
        assert!(!timeout.timed_out(), "the /slow clients never all arrived");
    }
}

struct Origin {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl Origin {
    fn start(addr: SocketAddr, state: Arc<OriginState>) -> Origin {
        // std sets SO_REUSEADDR on Unix, so a restart can take the same
        // port back while old connections sit in TIME_WAIT.
        let listener = TcpListener::bind(addr).unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let (state, stop) = (state.clone(), stop.clone());
            thread::spawn(move || {
                for conn in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(conn) = conn else { continue };
                    let state = state.clone();
                    thread::spawn(move || serve_origin(conn, &state));
                }
            })
        };
        Origin {
            addr,
            stop,
            accept: Some(accept),
        }
    }

    /// Closes the listener; the next connect is refused.
    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(accept) = self.accept.take() {
            accept.join().unwrap();
        }
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve_origin(mut conn: TcpStream, state: &OriginState) {
    conn.set_read_timeout(Some(TIMEOUT)).unwrap();
    let Some((method, target)) = read_request_line(&mut conn) else {
        return;
    };
    let path = target.split_once('?').map_or(target.as_str(), |(p, _)| p);
    let headers = conn_headers(&mut conn);
    if path != "/healthz" {
        *state
            .counts
            .lock()
            .unwrap()
            .entry(path.to_owned())
            .or_default() += 1;
    }
    let version = state.version.load(Ordering::SeqCst);
    let etag = format!("\"v{version}\"");
    let (status, extra, body): (&str, Vec<(&str, String)>, String) = match (method.as_str(), path) {
        ("GET", "/healthz") => ("200 OK", vec![], "ok\n".into()),
        ("POST", "/bump") => {
            state.version.fetch_add(1, Ordering::SeqCst);
            ("204 No Content", vec![], String::new())
        }
        ("GET", "/slow") => {
            state.wait_slow();
            (
                "200 OK",
                vec![("cache-control", "max-age=60".into())],
                "slow but shared\n".into(),
            )
        }
        ("GET", "/versioned") => {
            let stale = vec![
                ("cache-control", "max-age=2".into()),
                ("age", "10".into()),
                ("etag", etag.clone()),
            ];
            if headers.get("if-none-match") == Some(&etag) {
                ("304 Not Modified", stale, String::new())
            } else {
                ("200 OK", stale, format!("version {version}\n"))
            }
        }
        ("GET", "/news") => (
            "200 OK",
            vec![
                ("cache-control", "max-age=2, stale-if-error=600".into()),
                ("age", "10".into()),
                ("surrogate-key", "news".into()),
            ],
            format!("headlines, edition {version}\n"),
        ),
        ("GET", p) if p.starts_with("/static/") => (
            "200 OK",
            vec![
                ("cache-control", "public, max-age=300".into()),
                ("surrogate-key", "static".into()),
                ("content-type", "application/octet-stream".into()),
            ],
            format!("static {p}\n"),
        ),
        _ => ("404 Not Found", vec![], "not found\n".into()),
    };
    let mut out = format!("HTTP/1.1 {status}\r\nconnection: close\r\n");
    for (k, v) in extra {
        let _ = write!(out, "{k}: {v}\r\n");
    }
    if !status.starts_with("304") && !status.starts_with("204") {
        let _ = write!(out, "content-length: {}\r\n", body.len());
    }
    out.push_str("\r\n");
    out.push_str(&body);
    let _ = conn.write_all(out.as_bytes());
    let _ = conn.shutdown(Shutdown::Write);
}

fn read_line(conn: &mut TcpStream) -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match conn.read(&mut byte) {
            Ok(1) => {}
            _ => return None,
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line).ok()
}

fn read_request_line(conn: &mut TcpStream) -> Option<(String, String)> {
    let line = read_line(conn)?;
    let mut parts = line.split(' ');
    Some((parts.next()?.to_owned(), parts.next()?.to_owned()))
}

/// Reads header lines up to the blank line; GET and POST /bump carry no
/// body here.
fn conn_headers(conn: &mut TcpStream) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    while let Some(line) = read_line(conn) {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }
    headers
}

// ---------------------------------------------------------------------------
// The client side.

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Sends `request` (a full head without the final blank line) and reads
/// the response to the end of the connection.
fn send(addr: SocketAddr, request: &str, body: &str) -> Reply {
    let mut s = TcpStream::connect_timeout(&addr, TIMEOUT).unwrap();
    s.set_read_timeout(Some(TIMEOUT)).unwrap();
    write!(
        s,
        "{request}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    read_reply(s)
}

fn read_reply(mut s: TcpStream) -> Reply {
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8(raw).unwrap();
    let (head, rest) = text.split_once("\r\n\r\n").expect("a response head");
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap().split(' ').nth(1).unwrap();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.contains("chunked"));
    let body = if chunked {
        dechunk(rest)
    } else {
        rest.to_owned()
    };
    Reply {
        status: status.parse().unwrap(),
        headers,
        body,
    }
}

fn dechunk(mut rest: &str) -> String {
    let mut body = String::new();
    loop {
        let (size, after) = rest.split_once("\r\n").expect("a chunk size");
        let size = usize::from_str_radix(size.split(';').next().unwrap().trim(), 16).unwrap();
        if size == 0 {
            return body;
        }
        body.push_str(&after[..size]);
        rest = &after[size + 2..];
    }
}

fn get_request(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nhost: demo.example")
}

/// A free loopback port. The listener is dropped before the node binds it,
/// which leaves a small window for another process to take it.
fn free_addr() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

// ---------------------------------------------------------------------------
// The nodes.

struct Node {
    name: &'static str,
    proxy: SocketAddr,
    admin: SocketAddr,
    child: Child,
}

impl Node {
    fn start(
        bin: &Bin,
        dir: &std::path::Path,
        name: &'static str,
        admin: SocketAddr,
        origin: SocketAddr,
        peer: (&str, SocketAddr),
    ) -> Node {
        let config = format!(
            "node_id = \"{name}\"\nlisten = \"127.0.0.1:0\"\nadmin_listen = \"{admin}\"\n\
             [[origins]]\nname = \"origin\"\nurl = \"http://{origin}\"\n\
             [health]\ninterval_ms = 600000\n\
             [[peers]]\nname = \"{}\"\nurl = \"http://{}\"\n",
            peer.0, peer.1
        );
        let path = dir.join(format!("{name}.toml"));
        std::fs::write(&path, config).unwrap();
        let mut child = Command::new(bin.path)
            .arg("--config")
            .arg(&path)
            .env("CACHET_LOG", "warn,cachet::node=info")
            .env("CACHET_ADMIN_TOKEN", TOKEN)
            .env("CACHET_CLUSTER_SECRET", CLUSTER_SECRET)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let started = loop {
            let line = rx
                .recv_timeout(TIMEOUT)
                .unwrap_or_else(|_| panic!("{name} never logged its start"));
            if line.contains("\"message\":\"cachet started\"") {
                break line;
            }
        };
        let listen = started
            .split("\"listen\":\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .expect("a listen address");
        Node {
            name,
            proxy: listen.parse().unwrap(),
            admin,
            child,
        }
    }

    fn metric(&self, series: &str) -> Option<u64> {
        let reply = send(self.admin, "GET /metrics HTTP/1.1\r\nhost: admin", "");
        reply.body.lines().find_map(|l| {
            l.strip_prefix(series)
                .and_then(|v| v.strip_prefix(' '))
                .and_then(|v| v.parse().ok())
        })
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// The five steps.

struct Run {
    text: String,
}

impl Run {
    fn step(&mut self, title: &str) {
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        let _ = writeln!(self.text, "== {title}");
    }

    fn line(&mut self, line: &str) {
        self.text.push_str(line);
        self.text.push('\n');
    }

    /// GETs `path` from `node` and records the status, Cache-Status and body.
    fn get(&mut self, node: &Node, path: &str) -> Reply {
        let reply = send(node.proxy, &get_request(path), "");
        self.record(node.name, path, &reply);
        reply
    }

    fn record(&mut self, name: &str, path: &str, reply: &Reply) {
        let _ = writeln!(
            self.text,
            "{name} GET {path} -> {} | cache-status: {} | body: {:?}",
            reply.status,
            reply.header("cache-status").unwrap_or("<none>"),
            reply.body,
        );
    }

    fn origin_count(&mut self, state: &OriginState, path: &str) {
        let _ = writeln!(
            self.text,
            "origin requests for {path}: {}",
            state.count(path)
        );
    }
}

fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Sends `CLIENTS` concurrent GETs for `path` and returns their replies. The
/// origin is held until every client's request has reached the node.
fn concurrent(node: &Node, state: &Arc<OriginState>, path: &str) -> Vec<Reply> {
    let mut streams = Vec::new();
    for _ in 0..CLIENTS {
        let mut s = TcpStream::connect_timeout(&node.proxy, TIMEOUT).unwrap();
        s.set_read_timeout(Some(TIMEOUT)).unwrap();
        write!(
            s,
            "{}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            get_request(path)
        )
        .unwrap();
        streams.push(s);
    }
    // The first request has reached the origin; the node parses the others
    // and joins them to that fetch long before this wait ends.
    eventually("the leader reaches the origin", || state.count(path) == 1);
    thread::sleep(Duration::from_millis(500));
    state.open_slow();
    let readers: Vec<_> = streams
        .into_iter()
        .map(|s| thread::spawn(move || read_reply(s)))
        .collect();
    readers.into_iter().map(|r| r.join().unwrap()).collect()
}

/// A scratch directory removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs the five steps against `bin` and returns the transcript.
pub fn transcript(bin: &Bin) -> String {
    let dir = Scratch(std::env::temp_dir().join(format!(
        "e2e-workspace-{}-{}",
        bin.program,
        std::process::id()
    )));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();

    let state = Arc::new(OriginState::default());
    let mut origin = Origin::start(free_addr(), state.clone());
    let (a_admin, b_admin) = (free_addr(), free_addr());
    let a = Node::start(
        bin,
        &dir.0,
        "edge-a",
        a_admin,
        origin.addr,
        ("edge-b", b_admin),
    );
    let b = Node::start(
        bin,
        &dir.0,
        "edge-b",
        b_admin,
        origin.addr,
        ("edge-a", a_admin),
    );
    let mut run = Run {
        text: String::new(),
    };

    run.step("1. A miss is fetched and stored; the next request is a hit");
    run.get(&a, "/static/demo");
    run.get(&a, "/static/demo");
    run.origin_count(&state, "/static/demo");

    run.step("2. A stale object is revalidated with its entity tag");
    run.get(&a, "/versioned");
    run.get(&a, "/versioned");
    send(origin.addr, "POST /bump HTTP/1.1\r\nhost: origin", "");
    run.line("origin changed /versioned");
    run.get(&a, "/versioned");
    run.origin_count(&state, "/versioned");

    run.step("3. 50 concurrent requests for a cold URL make one origin request");
    let mut statuses: BTreeMap<String, usize> = BTreeMap::new();
    for reply in concurrent(&a, &state, "/slow") {
        let line = format!(
            "{} | cache-status: {} | body: {:?}",
            reply.status,
            reply.header("cache-status").unwrap_or("<none>"),
            reply.body
        );
        *statuses.entry(line).or_default() += 1;
    }
    for (line, n) in statuses {
        run.line(&format!("{n} x edge-a GET /slow -> {line}"));
    }
    run.origin_count(&state, "/slow");

    run.step("4. With the origin down, a stale object is served (stale-if-error)");
    run.get(&a, "/news");
    origin.stop();
    run.line("origin stopped");
    run.get(&a, "/news");
    run.get(&a, "/static/never-cached");
    let addr = origin.addr;
    drop(origin);
    let origin = Origin::start(addr, state.clone());
    run.line("origin restarted");

    run.step("5. A purge on edge-a reaches edge-b");
    run.get(&a, "/static/tagged");
    run.get(&b, "/static/tagged");
    run.get(&b, "/static/tagged");
    let reply = send(
        a.admin,
        &format!("POST /purge HTTP/1.1\r\nhost: admin\r\nauthorization: Bearer {TOKEN}"),
        r#"{"tags": ["static"]}"#,
    );
    // The reply carries a fresh purge id; everything after it is recorded.
    let after_id = reply.body.split_once("\",").map_or("", |(_, rest)| rest);
    run.line(&format!(
        "edge-a POST /purge {{\"tags\": [\"static\"]}} -> {} | {after_id}",
        reply.status
    ));
    eventually("edge-b applies the purge", || {
        b.metric("cachet_purges_peer_total") == Some(1)
    });
    run.line("edge-b applied the purge");
    run.get(&a, "/static/tagged");
    run.get(&b, "/static/tagged");
    run.origin_count(&state, "/static/tagged");
    drop(origin);
    run.text
}

/// A line diff: `-` lines are expected, `+` lines are what the binary did.
fn diff(want: &str, got: &str) -> String {
    let (want, got): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    let mut out = String::new();
    for i in 0..want.len().max(got.len()) {
        match (want.get(i), got.get(i)) {
            (Some(w), Some(g)) if w == g => {
                let _ = writeln!(out, "  {w}");
            }
            (w, g) => {
                if let Some(w) = w {
                    let _ = writeln!(out, "- {w}");
                }
                if let Some(g) = g {
                    let _ = writeln!(out, "+ {g}");
                }
            }
        }
    }
    out
}

/// Runs the steps against `bin` and fails with a line diff unless the
/// transcript equals the fixture byte for byte.
pub fn check(bin: &Bin) {
    let got = transcript(bin);
    if bin.program == "edgefront" && std::env::var_os("E2E_BLESS").is_some() {
        std::fs::write(bin.fixture, &got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(bin.fixture)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", bin.fixture));
    assert!(
        got == want,
        "{} differs from {}:\n{}",
        bin.program,
        bin.fixture,
        diff(&want, &got)
    );
}
