//! End to end: `edgefront proxy` serves steer's admin API on
//! `admin_listen`, with steer's token rules, reloads on `POST /reload` and
//! SIGHUP while endpoint state carries over, counts steer's metric series
//! on the request path, and drains on SIGTERM: `/readyz` turns 503 and
//! responses close their connections for the drain period, then the
//! process exits cleanly.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const EDGEFRONT: &str = env!("CARGO_BIN_EXE_edgefront");
const TIMEOUT: Duration = Duration::from_secs(10);
const TOKEN: &str = "an-admin-token-of-32-characters!";
/// steer's admin answers, recorded by `legacy/steer/internal/admin`.
const STEER_ADMIN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../edgefront-core/tests/testdata/admin_steer.json"
);

/// The member names of a JSON object, sorted.
fn keys(v: &serde_json::Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().expect("an object").keys().cloned().collect();
    k.sort();
    k
}

/// An upstream answering every request with `200 ok`, on its own thread.
fn upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { return };
            thread::spawn(move || {
                let mut w = conn.try_clone().unwrap();
                let mut r = BufReader::new(conn);
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

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("e2e-admin-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A configuration with one cluster `web` over `endpoints`, written to
/// `dir/proxy.json`.
fn write_config(dir: &Path, admin: &str, endpoints: &[&str], max_requests: u32) -> PathBuf {
    let eps: Vec<String> = endpoints
        .iter()
        .map(|a| format!("{{\"address\":\"{a}\"}}"))
        .collect();
    let text = format!(
        "{{\"listen\":\"127.0.0.1:0\",\"admin_listen\":\"{admin}\",\
         \"routes\":[{{\"host\":\"*\",\"path_prefix\":\"/\",\"cluster\":\"web\"}}],\
         \"clusters\":[{{\"name\":\"web\",\"max_requests\":{max_requests},\"endpoints\":[{}],\
         \"outlier_detection\":{{\"consecutive_gateway_failures\":2,\"interval\":\"10s\",\
         \"base_ejection_time\":\"60s\",\"max_ejection_percent\":50}},\
         \"retry\":{{\"backoff_base\":\"1ms\",\"backoff_max\":\"2ms\"}}}}]}}",
        eps.join(",")
    );
    let path = dir.join("proxy.json");
    std::fs::write(&path, text).unwrap();
    path
}

/// A running `edgefront proxy`, stopped with SIGTERM when dropped (so it
/// exits as it would in production, and writes its coverage profile under
/// `make cov`), and killed if it does not stop.
struct Proc {
    child: Child,
    proxy: String,
    admin: String,
    listening: String,
    lines: mpsc::Receiver<String>,
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = Command::new("kill")
                .args(["-TERM", &self.child.id().to_string()])
                .status();
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

    /// Waits for a log line containing `needle`.
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

    fn wait_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the proxy did not exit");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn field(line: &str, name: &str) -> String {
    let key = format!("\"{name}\":\"");
    let at = line
        .find(&key)
        .unwrap_or_else(|| panic!("{name} in {line}"))
        + key.len();
    line[at..].split('"').next().unwrap().to_owned()
}

fn start(config: &Path, env: &[(&str, &str)]) -> Proc {
    let mut cmd = Command::new(EDGEFRONT);
    cmd.args(["proxy", "--config"])
        .arg(config)
        .env_remove("EDGEFRONT_ADMIN_TOKEN")
        .env("EDGEFRONT_DRAIN_PERIOD", "0s")
        .env("EDGEFRONT_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("start the proxy");
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
        proxy: String::new(),
        admin: String::new(),
        listening: String::new(),
        lines,
    };
    let line = proc.wait_log("\"listening\"");
    proc.proxy = field(&line, "proxy");
    proc.admin = field(&line, "admin");
    proc.listening = line;
    proc
}

/// One exchange on its own connection: the status, the head and the body.
fn request(addr: &str, method: &str, path: &str, token: Option<&str>) -> (u16, String, String) {
    let mut conn = TcpStream::connect(addr).unwrap();
    conn.set_read_timeout(Some(TIMEOUT)).unwrap();
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    write!(
        conn,
        "{method} {path} HTTP/1.1\r\nHost: test\r\n{auth}Content-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = Vec::new();
    conn.read_to_end(&mut out).unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, head.to_owned(), body.to_owned())
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
}

fn clusters(p: &Proc) -> serde_json::Value {
    let (status, _, body) = request(&p.admin, "GET", "/clusters", Some(TOKEN));
    assert_eq!(status, 200, "{body}");
    json(&body)
}

fn metric(p: &Proc, series: &str) -> u64 {
    let (status, _, body) = request(&p.admin, "GET", "/metrics", Some(TOKEN));
    assert_eq!(status, 200, "{body}");
    body.lines()
        .find_map(|l| l.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
        .unwrap_or_else(|| panic!("no {series} in\n{body}"))
}

#[test]
fn the_admin_api_answers_with_the_live_proxy() {
    let up = upstream();
    let dir = scratch("api");
    let config = write_config(&dir, "127.0.0.1:0", &[&up], 64);
    let p = start(&config, &[("EDGEFRONT_ADMIN_TOKEN", TOKEN)]);
    assert!(
        p.listening.contains("\"admin_auth\":true"),
        "{}",
        p.listening
    );

    let (status, _, body) = request(&p.admin, "GET", "/healthz", None);
    assert_eq!((status, json(&body)["status"].clone()), (200, "ok".into()));
    let (status, _, body) = request(&p.admin, "GET", "/readyz", None);
    assert_eq!(
        (status, json(&body)["status"].clone()),
        (200, "ready".into())
    );
    for path in ["/clusters", "/metrics"] {
        let (status, head, body) = request(&p.admin, "GET", path, None);
        assert_eq!(status, 401, "{path}: {body}");
        assert!(
            head.contains("www-authenticate: Bearer realm=\"steer-admin\""),
            "{head}"
        );
        let (status, _, _) = request(
            &p.admin,
            "GET",
            path,
            Some("a-wrong-token-of-enough-length"),
        );
        assert_eq!(status, 401, "{path}");
    }
    let (status, _, _) = request(&p.admin, "POST", "/reload", None);
    assert_eq!(status, 401);

    for _ in 0..3 {
        let (status, _, body) = request(&p.proxy, "GET", "/hello", None);
        assert_eq!((status, body.as_str()), (200, "ok"));
    }
    let state = clusters(&p);
    assert_eq!(state["config_version"], 1);
    let web = &state["clusters"][0];
    assert_eq!(web["name"], "web");
    assert_eq!(web["policy"], "round_robin");
    assert_eq!(web["max_requests"], 64);
    assert_eq!(web["in_flight"], 0);
    assert_eq!(web["endpoints"][0]["address"], up.as_str());
    assert_eq!(web["endpoints"][0]["available"], true);
    assert!(web["retry_budget_balance"].is_number());

    assert_eq!(
        metric(
            &p,
            r#"steer_requests_total{cluster="web",code_class="2xx"}"#
        ),
        3
    );
    let attempts = format!(
        r#"steer_upstream_attempts_total{{cluster="web",endpoint="{up}",result="success"}}"#
    );
    assert_eq!(metric(&p, &attempts), 3);
    assert_eq!(metric(&p, "steer_config_version"), 1);
    let (_, head, _) = request(&p.admin, "GET", "/metrics", Some(TOKEN));
    assert!(
        head.contains("content-type: text/plain; version=0.0.4"),
        "{head}"
    );
}

#[test]
fn reloads_keep_endpoint_state_and_refuse_a_broken_file() {
    let up = upstream();
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let dir = scratch("reload");
    let config = write_config(&dir, "127.0.0.1:0", &[&dead, &up], 64);
    let p = start(&config, &[("EDGEFRONT_ADMIN_TOKEN", TOKEN)]);
    // Every request still succeeds through the retry; two connect errors
    // eject the dead endpoint.
    for _ in 0..6 {
        let (status, _, body) = request(&p.proxy, "GET", "/", None);
        assert_eq!((status, body.as_str()), (200, "ok"));
    }
    let state = clusters(&p);
    assert_eq!(state["clusters"][0]["endpoints"][0]["ejected"], true);

    // The live document has the members steer's had, level by level: an
    // ejected endpoint and an available one, a cluster with a retry
    // budget, and the balancer.
    let cases: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(STEER_ADMIN).unwrap()).unwrap();
    let steer = cases
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "clusters")
        .expect("the clusters case");
    let steer = json(steer["body"].as_str().unwrap());
    let (ours, theirs) = (&state["clusters"][0], &steer["clusters"][0]);
    assert_eq!(keys(&state), keys(&steer));
    assert_eq!(keys(ours), keys(theirs));
    assert_eq!(keys(&ours["balancer"]), keys(&theirs["balancer"]));
    assert_eq!(
        keys(&ours["endpoints"][0]),
        keys(&theirs["endpoints"][1]),
        "ejected"
    );
    assert_eq!(
        keys(&ours["endpoints"][1]),
        keys(&theirs["endpoints"][0]),
        "available"
    );
    assert_eq!(
        ours["endpoints"][0]["last_eject_reason"],
        "consecutive_gateway_failure"
    );

    let (status, _, body) = request(&p.admin, "POST", "/reload", Some(TOKEN));
    assert_eq!(status, 200, "{body}");
    let doc = json(&body);
    assert_eq!(
        (doc["status"].clone(), doc["config_version"].clone()),
        ("reloaded".into(), 2.into())
    );
    p.wait_log("\"configuration loaded\"");

    write_config(&dir, "127.0.0.1:0", &[&dead, &up], 7);
    p.signal("HUP");
    p.wait_log("\"reloaded on SIGHUP\"");
    let state = clusters(&p);
    assert_eq!(state["config_version"], 3);
    let web = &state["clusters"][0];
    assert_eq!(web["max_requests"], 7);
    // The ejection outlived both reloads.
    assert_eq!(web["endpoints"][0]["ejected"], true);
    assert_eq!(web["endpoints"][0]["times_ejected"], 1);

    std::fs::write(&config, "{\"listen\": 5}").unwrap();
    let (status, _, body) = request(&p.admin, "POST", "/reload", Some(TOKEN));
    assert_eq!(status, 400, "{body}");
    assert_eq!(json(&body)["error"]["code"], "invalid_config");
    p.wait_log("configuration rejected; the previous configuration keeps serving");
    p.signal("HUP");
    p.wait_log("configuration rejected; the previous configuration keeps serving");
    assert_eq!(clusters(&p)["config_version"], 3);
    let (status, _, _) = request(&p.proxy, "GET", "/", None);
    assert_eq!(status, 200);
    assert_eq!(
        metric(&p, r#"steer_config_reloads_total{result="success"}"#),
        2
    );
    assert_eq!(
        metric(&p, r#"steer_config_reloads_total{result="failure"}"#),
        2
    );
    assert_eq!(metric(&p, "steer_config_version"), 3);
}

#[test]
fn sigterm_drains_then_stops() {
    let up = upstream();
    let dir = scratch("drain");
    let config = write_config(&dir, "127.0.0.1:0", &[&up], 64);
    let mut p = start(&config, &[("EDGEFRONT_DRAIN_PERIOD", "1500ms")]);
    assert!(
        p.listening.contains("\"admin_auth\":false"),
        "{}",
        p.listening
    );
    let (status, _, _) = request(&p.proxy, "GET", "/", None);
    assert_eq!(status, 200);

    p.signal("TERM");
    p.wait_log("\"draining\"");
    let (status, _, body) = request(&p.admin, "GET", "/readyz", None);
    assert_eq!(
        (status, json(&body)["status"].clone()),
        (503, "draining".into())
    );
    // The listener still serves during the drain period, closing each
    // connection after its response.
    let mut conn = TcpStream::connect(&p.proxy).unwrap();
    conn.set_read_timeout(Some(TIMEOUT)).unwrap();
    conn.write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
        .unwrap();
    let mut out = Vec::new();
    conn.read_to_end(&mut out)
        .expect("the proxy closes the connection");
    let text = String::from_utf8_lossy(&out);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.contains("connection: close"), "{text}");

    p.wait_log("\"shutting down\"");
    p.wait_log("\"stopped\"");
    assert!(p.wait_exit().success());
}

#[test]
fn the_shutdown_timeout_cuts_what_is_still_in_flight() {
    // An upstream that takes requests and never answers.
    let hole = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = hole.local_addr().unwrap().to_string();
    thread::spawn(move || {
        let mut held = Vec::new();
        for conn in hole.incoming().flatten() {
            held.push(conn);
        }
    });
    let dir = scratch("cut");
    let config = write_config(&dir, "127.0.0.1:0", &[&addr], 64);
    let mut p = start(&config, &[("EDGEFRONT_SHUTDOWN_TIMEOUT", "200ms")]);
    let mut conn = TcpStream::connect(&p.proxy).unwrap();
    conn.write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
        .unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while clusters(&p)["clusters"][0]["in_flight"] != 1 {
        assert!(
            Instant::now() < deadline,
            "the request never reached the upstream"
        );
        thread::sleep(Duration::from_millis(20));
    }
    p.signal("TERM");
    let line = p.wait_log("shutdown timeout reached; closing remaining connections");
    assert!(line.contains("\"listener\":\"proxy\""), "{line}");
    p.wait_log("\"stopped\"");
    assert!(p.wait_exit().success());
}

fn refused(config: &Path, env: &[(&str, &str)]) -> String {
    let mut cmd = Command::new(EDGEFRONT);
    cmd.args(["proxy", "--config"])
        .arg(config)
        .env_remove("EDGEFRONT_ADMIN_TOKEN");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn settings_are_checked_before_anything_listens() {
    let up = upstream();
    let dir = scratch("settings");
    let open = write_config(&dir, "0.0.0.0:0", &[&up], 64);
    assert_eq!(
        refused(&open, &[]),
        "edgefront: admin_listen 0.0.0.0:0 is not a loopback address; \
         set EDGEFRONT_ADMIN_TOKEN to expose the admin API\n"
    );
    assert_eq!(
        refused(&open, &[("EDGEFRONT_ADMIN_TOKEN", "too-short")]),
        "edgefront: EDGEFRONT_ADMIN_TOKEN must be at least 16 characters\n"
    );
    assert_eq!(
        refused(
            &open,
            &[
                ("EDGEFRONT_ADMIN_TOKEN", TOKEN),
                ("EDGEFRONT_SHUTDOWN_TIMEOUT", "2h")
            ]
        ),
        "edgefront: EDGEFRONT_SHUTDOWN_TIMEOUT must be a duration between 0s and 1h0m0s\n"
    );
    // With a token the admin API may listen on every interface.
    let p = start(&open, &[("EDGEFRONT_ADMIN_TOKEN", TOKEN)]);
    let port = p.admin.rsplit(':').next().unwrap();
    let (status, _, _) = request(
        &format!("127.0.0.1:{port}"),
        "GET",
        "/clusters",
        Some(TOKEN),
    );
    assert_eq!(status, 200);
}
