//! End to end: the edgefront binary proxies HTTP/1.1 with steer's example
//! configuration and behaves as steer did.
//!
//! Three fake upstreams (`a` and `b` at priority 0, `c` at priority 1, as in
//! `legacy/steer/examples/steer.json`) answer the way steer's
//! `internal/fakeupstream` does and record every request they receive.
//! Each case is sent as raw bytes on its own connection: the README's
//! traffic (`GET /hello`) and the request-path behaviour the README lists,
//! including path cleaning, forwarding headers from an untrusted client,
//! request IDs, hop-by-hop headers in both directions, body framing,
//! upstream errors passed through, and the requests Go's server or steer
//! refuse.
//!
//! A case's transcript holds the response the client got and the requests
//! the upstreams got, header values byte for byte. A few things are
//! normalised because they change between runs or proxies: the `Date`
//! header, the framing headers (`Content-Length` and `Transfer-Encoding` on
//! the response; bodies are compared decoded), generated request IDs, the
//! upstreams' ports, and which of the two equal priority-0 upstreams was
//! picked.
//!
//! The binary's transcript must equal `tests/fixtures/e2e_port_http1.txt`,
//! which was recorded from steer. With `STEER_BIN` pointing at a build of
//! `legacy/steer/cmd/steer` (`make e2e-steer`, run in CI by the
//! legacy-steer workflow), steer runs the same cases live and its
//! transcript must equal the file too; `E2E_BLESS=1` rewrites the file from
//! steer.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const EDGEFRONT: &str = env!("CARGO_BIN_EXE_edgefront");
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/e2e_port_http1.txt"
);
const STEER_EXAMPLE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../legacy/steer/examples/steer.json"
);
const TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// HTTP/1.1 on the wire, just enough for both ends of the test.

struct Head {
    start: String,
    headers: Vec<(String, Vec<u8>)>,
}

impl Head {
    fn get(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_slice())
    }

    fn has_token(&self, name: &str, token: &str) -> bool {
        self.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .flat_map(|(_, v)| v.split(|&c| c == b','))
            .any(|t| t.trim_ascii().eq_ignore_ascii_case(token.as_bytes()))
    }
}

fn read_line(r: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    if r.read_until(b'\n', &mut line)? == 0 {
        return Ok(None);
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    Ok(Some(line))
}

/// Reads a request or response head; `None` at a clean end of stream.
fn read_head(r: &mut impl BufRead) -> io::Result<Option<Head>> {
    let Some(start) = read_line(r)? else {
        return Ok(None);
    };
    let mut headers = Vec::new();
    loop {
        let line = read_line(r)?.ok_or(io::ErrorKind::UnexpectedEof)?;
        if line.is_empty() {
            break;
        }
        let colon = line.iter().position(|&c| c == b':').expect("a header line");
        let name = String::from_utf8(line[..colon].to_vec()).unwrap();
        headers.push((name, line[colon + 1..].trim_ascii().to_vec()));
    }
    Ok(Some(Head {
        start: String::from_utf8_lossy(&start).into_owned(),
        headers,
    }))
}

/// Reads a message body: chunked, by length, or (for a response) until the
/// connection closes.
fn read_body(r: &mut impl BufRead, head: &Head, to_eof: bool) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    if head.has_token("transfer-encoding", "chunked") {
        loop {
            let line = read_line(r)?.ok_or(io::ErrorKind::UnexpectedEof)?;
            let size = line.split(|&c| c == b';').next().unwrap();
            let size = usize::from_str_radix(std::str::from_utf8(size).unwrap().trim(), 16)
                .expect("a chunk size");
            if size == 0 {
                while !read_line(r)?
                    .ok_or(io::ErrorKind::UnexpectedEof)?
                    .is_empty()
                {}
                return Ok(body);
            }
            let at = body.len();
            body.resize(at + size, 0);
            r.read_exact(&mut body[at..])?;
            read_line(r)?;
        }
    }
    if let Some(n) = head.get("content-length") {
        let n: usize = std::str::from_utf8(n).unwrap().parse().unwrap();
        body.resize(n, 0);
        r.read_exact(&mut body)?;
    } else if to_eof {
        r.read_to_end(&mut body)?;
    }
    Ok(body)
}

fn chunked(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(format!("{:x}\r\n", p.len()).as_bytes());
        out.extend_from_slice(p);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

/// Go's `url.PathUnescape` for the simple paths the fake upstream sees.
fn unescape(p: &str) -> String {
    let b = p.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&p[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// The fake upstreams.

/// What an upstream received for one request.
struct Received {
    upstream: &'static str,
    head: Head,
    body: Vec<u8>,
}

type Log = Arc<Mutex<HashMap<String, Vec<Received>>>>;

struct Upstream {
    name: &'static str,
    addr: String,
}

fn start_upstream(name: &'static str, log: Log) -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let log = log.clone();
            thread::spawn(move || {
                let _ = serve_upstream(name, conn, &log);
            });
        }
    });
    Upstream { name, addr }
}

fn serve_upstream(name: &'static str, conn: TcpStream, log: &Log) -> io::Result<()> {
    let mut w = conn.try_clone()?;
    let mut r = BufReader::new(conn);
    while let Some(head) = read_head(&mut r)? {
        let body = read_body(&mut r, &head, false)?;
        let mut words = head.start.split(' ');
        let method = words.next().unwrap_or_default().to_owned();
        let target = words.next().unwrap_or_default();
        let path = unescape(target.split('?').next().unwrap_or_default());
        let close = head.has_token("connection", "close");

        let json =
            format!("{{\"method\":\"{method}\",\"path\":\"{path}\",\"upstream\":\"{name}\"}}\n");
        let (status, extra, payload): (&str, &str, Vec<u8>) = match path.as_str() {
            "/healthz" => ("200 OK", "", b"{\"status\":\"ok\"}\n".to_vec()),
            "/hop" => (
                "200 OK",
                "Connection: X-Secret\r\nX-Secret: hop\r\nKeep-Alive: timeout=5\r\nProxy-Authenticate: Basic\r\nX-Kept: yes\r\n",
                json.into_bytes(),
            ),
            "/fail" => (
                "503 Service Unavailable",
                "",
                format!("{{\"error\":\"injected failure\",\"upstream\":\"{name}\"}}\n")
                    .into_bytes(),
            ),
            "/error500" => (
                "500 Internal Server Error",
                "",
                format!("{{\"error\":\"boom\",\"upstream\":\"{name}\"}}\n").into_bytes(),
            ),
            _ => ("200 OK", "", json.into_bytes()),
        };

        if let Some(case) = head.get("x-case") {
            let case = String::from_utf8_lossy(case).into_owned();
            log.lock().unwrap().entry(case).or_default().push(Received {
                upstream: name,
                head,
                body,
            });
        }

        let mut out = Vec::new();
        if path == "/empty" {
            write!(out, "HTTP/1.1 204 No Content\r\nX-Upstream: {name}\r\n\r\n")?;
        } else if path == "/chunked" {
            write!(
                out,
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Upstream: {name}\r\nTransfer-Encoding: chunked\r\n\r\n"
            )?;
            out.extend_from_slice(&chunked(&[b"one ", b"two ", b"three\n"]));
        } else {
            write!(
                out,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nX-Upstream: {name}\r\n{extra}Content-Length: {}\r\n\r\n",
                payload.len()
            )?;
            if method != "HEAD" {
                out.extend_from_slice(&payload);
            }
        }
        w.write_all(&out)?;
        if close {
            break;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The proxies under test.

/// A proxy process, killed when dropped.
struct Proc {
    child: Child,
    addr: String,
    lines: mpsc::Receiver<String>,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts a proxy and waits for its `listening` log line, which both
/// binaries write with the bound address in `"proxy"`.
fn spawn(mut cmd: Command) -> Proc {
    let child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start the proxy");
    let (tx, rx) = mpsc::channel();
    // Owned by `Proc` from here, so a failed wait below still kills it.
    let mut proc = Proc {
        child,
        addr: String::new(),
        lines: rx,
    };
    let stdout = proc.child.stdout.take().unwrap();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    loop {
        let line = proc.lines.recv_timeout(TIMEOUT).expect("a listening line");
        if line.contains("\"listening\"") {
            let at = line.find("\"proxy\":\"").expect("the proxy address") + 9;
            proc.addr = line[at..].split('"').next().unwrap().to_owned();
            return proc;
        }
    }
}

/// steer's example configuration, listening on any free port and pointing
/// at the fake upstreams.
fn example_config(dir: &Path, upstreams: &[Upstream]) -> PathBuf {
    let text = std::fs::read_to_string(STEER_EXAMPLE).unwrap();
    let mut cfg: serde_json::Value = serde_json::from_str(&text).unwrap();
    cfg["listen"] = "127.0.0.1:0".into();
    cfg["admin_listen"] = "127.0.0.1:0".into();
    let endpoints = cfg["clusters"][0]["endpoints"].as_array_mut().unwrap();
    assert_eq!(endpoints.len(), upstreams.len());
    for (e, u) in endpoints.iter_mut().zip(upstreams) {
        e["address"] = u.addr.clone().into();
    }
    let path = dir.join("steer.json");
    std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
    path
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("e2e-http1-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// The cases.

struct Case {
    name: &'static str,
    request: Vec<u8>,
    /// Whether the client sent an `X-Request-Id` the proxy keeps.
    keeps_id: bool,
}

fn case(name: &'static str, head: &str, body: &[u8]) -> Case {
    let mut request = head.replace("{case}", name).into_bytes();
    request.extend_from_slice(body);
    Case {
        name,
        request,
        keeps_id: false,
    }
}

const CURL: &str = "User-Agent: curl/8.5.0\r\nAccept: */*\r\n";

fn cases() -> Vec<Case> {
    let big: Vec<u8> = (0..70_000u32).map(|i| b'a' + (i % 26) as u8).collect();
    let mut kept_id = case(
        "request_id_kept",
        "GET /hello HTTP/1.1\r\nHost: localhost:18080\r\nX-Request-Id: abc-123.x_y:z\r\nX-Case: {case}\r\n\r\n",
        b"",
    );
    kept_id.keeps_id = true;
    vec![
        case(
            "readme_hello",
            &format!(
                "GET /hello HTTP/1.1\r\nHost: localhost:18080\r\n{CURL}X-Case: {{case}}\r\n\r\n"
            ),
            b"",
        ),
        case(
            "head",
            &format!(
                "HEAD /hello HTTP/1.1\r\nHost: localhost:18080\r\n{CURL}X-Case: {{case}}\r\n\r\n"
            ),
            b"",
        ),
        case(
            "post_json",
            "POST /orders HTTP/1.1\r\nHost: localhost:18080\r\nContent-Type: application/json\r\nContent-Length: 13\r\nX-Case: {case}\r\n\r\n",
            b"{\"sku\":\"a-1\"}",
        ),
        case(
            "post_chunked",
            "POST /upload HTTP/1.1\r\nHost: localhost:18080\r\nTransfer-Encoding: chunked\r\nX-Case: {case}\r\n\r\n",
            &chunked(&[b"hello ", b"world"]),
        ),
        case(
            "post_chunked_large",
            "POST /upload HTTP/1.1\r\nHost: localhost:18080\r\nTransfer-Encoding: chunked\r\nX-Case: {case}\r\n\r\n",
            &chunked(&[&big[..40_000], &big[40_000..]]),
        ),
        case(
            "post_large",
            "POST /upload HTTP/1.1\r\nHost: localhost:18080\r\nContent-Length: 70000\r\nX-Case: {case}\r\n\r\n",
            &big,
        ),
        case(
            "put_empty",
            "PUT /items/1 HTTP/1.1\r\nHost: localhost:18080\r\nContent-Length: 0\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "get_content_length_zero",
            "GET /items/1 HTTP/1.1\r\nHost: localhost:18080\r\nContent-Length: 0\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "dot_segments",
            "GET /public/../admin HTTP/1.1\r\nHost: localhost:18080\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "encoded_dot_segments",
            "GET /public%2F..%2Fadmin HTTP/1.1\r\nHost: localhost:18080\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "encoded_slash_kept",
            "GET /a%2Fb?x=1&y=%20 HTTP/1.1\r\nHost: localhost:18080\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "double_slashes",
            "GET //double//slashes/ HTTP/1.1\r\nHost: localhost:18080\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "bare_query",
            "GET /x? HTTP/1.1\r\nHost: localhost:18080\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "options_star",
            "OPTIONS * HTTP/1.1\r\nHost: localhost:18080\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "hop_by_hop",
            "GET /hop HTTP/1.1\r\nHost: localhost:18080\r\nConnection: X-Hop, Keep-Alive\r\nX-Hop: secret\r\nKeep-Alive: 300\r\nTE: trailers, deflate\r\nUpgrade: h2c\r\nProxy-Authorization: Basic eDp5\r\nProxy-Connection: keep-alive\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "spoofed_forwarding",
            "GET /hello HTTP/1.1\r\nHost: localhost:18080\r\nX-Forwarded-For: 1.2.3.4\r\nX-Forwarded-Proto: https\r\nX-Forwarded-Host: evil.example\r\nForwarded: for=1.2.3.4\r\nX-Real-Ip: 1.2.3.4\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "host_case_and_port",
            "GET /hello HTTP/1.1\r\nHost: Shop.Example.COM:18080\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        kept_id,
        case(
            "request_id_replaced",
            "GET /hello HTTP/1.1\r\nHost: localhost:18080\r\nX-Request-Id: has space\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "upstream_503_passed_through",
            "POST /fail HTTP/1.1\r\nHost: localhost:18080\r\nContent-Length: 2\r\nX-Case: {case}\r\n\r\n",
            b"{}",
        ),
        case(
            "upstream_500_passed_through",
            "GET /error500 HTTP/1.1\r\nHost: localhost:18080\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "chunked_response",
            "GET /chunked HTTP/1.1\r\nHost: localhost:18080\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "no_content",
            "GET /empty HTTP/1.1\r\nHost: localhost:18080\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "absolute_form",
            "GET http://Shop.Example.com/abs?q=1 HTTP/1.1\r\nHost: localhost:18080\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "http10_without_host",
            "GET /old HTTP/1.0\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "connect_refused",
            "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "bad_escape",
            "GET /bad%zz HTTP/1.1\r\nHost: localhost:18080\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "missing_host",
            "GET /hello HTTP/1.1\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "two_hosts",
            "GET /hello HTTP/1.1\r\nHost: a\r\nHost: b\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
        case(
            "malformed_host",
            "GET /hello HTTP/1.1\r\nHost: a<b\r\nX-Case: {case}\r\n\r\n",
            b"",
        ),
    ]
}

// ---------------------------------------------------------------------------
// Transcripts.

fn show(bytes: &[u8]) -> String {
    if bytes.len() > 200 {
        let sum = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        });
        return format!("<{} bytes, fnv1a {sum:016x}>", bytes.len());
    }
    bytes.escape_ascii().to_string()
}

/// Headers sorted by lower-case name, keeping the order of repeated ones.
fn sorted(headers: &[(String, Vec<u8>)], skip: &[&str]) -> Vec<(String, String)> {
    let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (n, v) in headers {
        let n = n.to_ascii_lowercase();
        if !skip.contains(&n.as_str()) {
            by_name.entry(n).or_default().push(show(v));
        }
    }
    by_name
        .into_iter()
        .flat_map(|(n, vs)| vs.into_iter().map(move |v| (n.clone(), v)))
        .collect()
}

fn run_cases(proxy: &Proc, upstreams: &[Upstream], log: &Log) -> String {
    let mut out = String::new();
    for c in cases() {
        let conn = TcpStream::connect(&proxy.addr).unwrap();
        conn.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut w = conn.try_clone().unwrap();
        w.write_all(&c.request).unwrap();
        let mut r = BufReader::new(conn);
        let head = read_head(&mut r).unwrap().expect("a response");
        let status: u16 = head.start[9..12].parse().unwrap();
        let no_body = c.request.starts_with(b"HEAD ") || status == 204 || status == 304;
        let body = if no_body {
            Vec::new()
        } else {
            let close = head.has_token("connection", "close");
            read_body(&mut r, &head, close).unwrap()
        };
        let _ = w.shutdown(Shutdown::Both);

        let mut text = String::new();
        let line_end = c.request.iter().position(|&b| b == b'\r').unwrap();
        writeln!(text, "== {}", c.name).unwrap();
        writeln!(text, "> {}", show(&c.request[..line_end])).unwrap();
        writeln!(text, "< {}", &head.start[9..]).unwrap();
        for (n, v) in sorted(
            &head.headers,
            &["date", "content-length", "transfer-encoding"],
        ) {
            writeln!(text, "< {n}: {v}").unwrap();
        }
        writeln!(text, "< body: {}", show(&body)).unwrap();
        let received = log.lock().unwrap().remove(c.name).unwrap_or_default();
        writeln!(text, "upstream requests: {}", received.len()).unwrap();
        for rec in &received {
            writeln!(text, "| at {}", rec.upstream).unwrap();
            writeln!(text, "| {}", rec.head.start).unwrap();
            for (n, v) in sorted(&rec.head.headers, &["x-case"]) {
                writeln!(text, "| {n}: {v}").unwrap();
            }
            writeln!(text, "| body: {}", show(&rec.body)).unwrap();
        }

        // Normalise what changes between runs.
        if !c.keeps_id
            && let Some(id) = head.get("x-request-id")
        {
            let id = std::str::from_utf8(id).unwrap();
            assert_eq!(id.len(), 24, "{}: generated id {id}", c.name);
            assert!(
                id.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            );
            text = text.replace(id, "{generated-id}");
        }
        for u in upstreams {
            text = text.replace(&u.addr, &format!("{{endpoint-{}}}", u.name));
        }
        for primary in ["a", "b"] {
            text = text
                .replace(
                    &format!(r#"\"upstream\":\"{primary}\""#),
                    r#"\"upstream\":\"{a|b}\""#,
                )
                .replace(&format!("{{endpoint-{primary}}}"), "{endpoint-a|b}")
                .replace(
                    &format!("< x-upstream: {primary}\n"),
                    "< x-upstream: {a|b}\n",
                )
                .replace(&format!("| at {primary}\n"), "| at {a|b}\n");
        }
        out.push_str(&text);
        out.push('\n');
    }
    out
}

fn upstreams(log: &Log) -> Vec<Upstream> {
    ["a", "b", "c"]
        .into_iter()
        .map(|n| start_upstream(n, log.clone()))
        .collect()
}

fn first_difference(want: &str, got: &str) -> String {
    for (i, (w, g)) in want.lines().zip(got.lines()).enumerate() {
        if w != g {
            return format!("line {}:\n  want: {w}\n  got:  {g}", i + 1);
        }
    }
    format!(
        "lengths differ: want {} lines, got {}",
        want.lines().count(),
        got.lines().count()
    )
}

#[cfg(unix)]
fn terminate(proc: &mut Proc) -> std::process::ExitStatus {
    let pid = proc.child.id().to_string();
    let status = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
    assert!(status.success());
    for _ in 0..100 {
        if let Some(status) = proc.child.try_wait().unwrap() {
            return status;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("the proxy did not stop on SIGTERM");
}

#[test]
fn matches_steer() {
    let log: Log = Arc::default();
    let ups = upstreams(&log);
    let dir = scratch("edgefront");
    let cfg = example_config(&dir, &ups);

    let mut cmd = Command::new(EDGEFRONT);
    cmd.arg("proxy")
        .arg("--config")
        .arg(&cfg)
        .env_remove("EDGEFRONT_LOG");
    #[allow(unused_mut)]
    let mut proxy = spawn(cmd);
    let got = run_cases(&proxy, &ups, &log);

    if let Ok(steer) = std::env::var("STEER_BIN") {
        let sdir = scratch("steer");
        let scfg = example_config(&sdir, &ups);
        let mut scmd = Command::new(steer);
        scmd.env("STEER_CONFIG", &scfg)
            .env("STEER_DRAIN_PERIOD", "0s")
            .env_remove("STEER_ADMIN_TOKEN");
        let sproxy = spawn(scmd);
        let live = run_cases(&sproxy, &ups, &log);
        drop(sproxy);
        let _ = std::fs::remove_dir_all(&sdir);
        if std::env::var_os("E2E_BLESS").is_some() {
            std::fs::write(FIXTURE, &live).unwrap();
        }
        let recorded = std::fs::read_to_string(FIXTURE).unwrap();
        assert!(
            live == recorded,
            "steer no longer matches the recording, {}",
            first_difference(&recorded, &live)
        );
    }

    let want = std::fs::read_to_string(FIXTURE).unwrap();
    assert!(
        got == want,
        "edgefront differs from steer, {}\n\nfull transcript:\n{got}",
        first_difference(&want, &got)
    );

    // Every request was answered by the logged handler, and SIGTERM stops
    // the proxy cleanly.
    #[cfg(unix)]
    {
        let status = terminate(&mut proxy);
        assert!(status.success(), "{status}");
        let lines: Vec<String> = proxy.lines.try_iter().collect();
        let requests = lines
            .iter()
            .filter(|l| l.contains("\"message\":\"request\""))
            .count();
        // What Go's server answers itself (`OPTIONS *` and four refusals)
        // never reaches the handler.
        assert_eq!(requests, cases().len() - 5, "{lines:#?}");
        assert!(
            lines.iter().any(|l| l.contains("\"message\":\"stopped\"")),
            "{lines:#?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// The command line.

fn edgefront(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(EDGEFRONT)
        .args(args)
        .env_remove("EDGEFRONT_LOG")
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[test]
fn command_line() {
    let usage =
        "usage: edgefront proxy --config <file>\n       edgefront proxy --check-config <file>\n";
    for args in [
        &["proxy"][..],
        &["proxy", "--config"],
        &["proxy", "--bogus", "x"],
        &["proxy", "--config", "a", "b"],
    ] {
        assert_eq!(
            edgefront(args),
            (2, String::new(), usage.to_owned()),
            "{args:?}"
        );
    }

    let dir = scratch("cli");
    let example = STEER_EXAMPLE;
    assert_eq!(
        edgefront(&["proxy", "--check-config", example]),
        (0, format!("{example}: ok\n"), String::new())
    );

    let missing = dir.join("missing.json");
    let missing = missing.to_str().unwrap();
    let (code, out, err) = edgefront(&["proxy", "--check-config", missing]);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.starts_with(&format!("edgefront: {missing}: ")), "{err}");

    let invalid = dir.join("invalid.json");
    std::fs::write(&invalid, r#"{"routes":[],"clusters":[]}"#).unwrap();
    let invalid = invalid.to_str().unwrap();
    let (code, _, err) = edgefront(&["proxy", "--config", invalid]);
    assert_eq!(code, 2);
    assert!(err.starts_with(&format!("edgefront: {invalid}: ")), "{err}");

    let tls = dir.join("tls.json");
    let cert = dir.join("c.pem");
    std::fs::write(&cert, "").unwrap();
    let mut cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(STEER_EXAMPLE).unwrap()).unwrap();
    cfg["tls"] = serde_json::json!({"certificates": [{"cert_file": "c.pem", "key_file": "c.pem"}]});
    std::fs::write(&tls, cfg.to_string()).unwrap();
    let tls = tls.to_str().unwrap();
    assert_eq!(
        edgefront(&["proxy", "--check-config", tls]),
        (
            2,
            String::new(),
            format!(
                "edgefront: {tls}: tls is not supported by the proxy yet; remove the tls block\n"
            )
        )
    );

    // A listen address in use: the proxy cannot start.
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(STEER_EXAMPLE).unwrap()).unwrap();
    cfg["listen"] = taken.local_addr().unwrap().to_string().into();
    let busy = dir.join("busy.json");
    std::fs::write(&busy, cfg.to_string()).unwrap();
    let (code, out, _) = edgefront(&["proxy", "--config", busy.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(out.contains("\"message\":\"cannot listen\""), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn listens_on_every_interface_for_an_empty_host() {
    // Go's ":port" form. Port 0 picks a free port on [::] (or 0.0.0.0).
    let log: Log = Arc::default();
    let ups = upstreams(&log);
    let dir = scratch("anyhost");
    let cfg = example_config(&dir, &ups);
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    v["listen"] = ":0".into();
    std::fs::write(&cfg, v.to_string()).unwrap();
    let mut cmd = Command::new(EDGEFRONT);
    cmd.arg("proxy")
        .arg("--config")
        .arg(&cfg)
        .env_remove("EDGEFRONT_LOG");
    let proxy = spawn(cmd);
    let port = proxy.addr.rsplit(':').next().unwrap();
    let mut conn = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    conn.set_read_timeout(Some(TIMEOUT)).unwrap();
    conn.write_all(b"GET /hello HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut resp = String::new();
    conn.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "{resp}");
    assert!(
        resp.ends_with("\"path\":\"/hello\",\"upstream\":\"a\"}\n")
            || resp.ends_with("\"upstream\":\"b\"}\n"),
        "{resp}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
