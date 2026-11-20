//! End to end: steer's failover drill against the edgefront binary.
//!
//! Two priority-0 upstreams and one priority-1 backup sit behind `edgefront
//! proxy` while load runs at 200 requests per second, and each 10-second
//! phase adds one fault while the load runs: the baseline, primary-a
//! killed, primary-b failing half its requests, then primary-b blackholed.
//! It is `examples/failover.sh` in process: the upstreams and the load
//! generator are `edgefront_difftest::drill`'s ports of steer's
//! `cmd/upstream` and `cmd/loadgen`, and the configurations are the ones
//! in `examples/failover/`, steer's own, with the addresses moved to free
//! ports.
//!
//! With health checks, outlier detection and retries on, every phase must
//! succeed 100.00%, as in steer's published table, and the tail must stay
//! well inside the cluster timeout: the per-try timeout moves a request
//! off the blackholed primary after 250 ms. With them off, the baseline
//! still succeeds 100.00% and every fault shows, which proves the drill
//! breaks what it says it breaks.
//!
//! The resilient drill runs a second time over HTTPS with mutual TLS: the
//! upstreams terminate TLS with a certificate for `api.example.com` and
//! require a client certificate from the fixture CA, and the cluster's
//! `tls` block pins that CA, names the server and presents the client
//! certificate, for requests and health checks alike. It must hold the
//! same table.
//!
//! `EDGEFRONT_E2E_PHASE_SECONDS` shortens or lengthens the phases.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use edgefront_difftest::drill::{LoadOptions, Report, Upstream, load};

const EDGEFRONT: &str = env!("CARGO_BIN_EXE_edgefront");
const TIMEOUT: Duration = Duration::from_secs(10);
const RATE: f64 = 200.0;
const PHASES: [&str; 4] = [
    "1. baseline",
    "2. kill primary-a",
    "3. primary-b 50% errors",
    "4. blackhole primary-b",
];

fn phase_seconds() -> u64 {
    std::env::var("EDGEFRONT_E2E_PHASE_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&s| s > 0)
        .unwrap_or(10)
}

/// `examples/failover/<name>.json` with the proxy and admin listeners on
/// free ports and the endpoints on `upstreams`, written to a scratch file.
fn config(name: &str, upstreams: &[String; 3], tls: bool) -> std::path::PathBuf {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/failover");
    let text = std::fs::read_to_string(examples.join(format!("{name}.json"))).unwrap();
    let mut c: serde_json::Value = serde_json::from_str(&text).unwrap();
    c["listen"] = "127.0.0.1:0".into();
    c["admin_listen"] = "127.0.0.1:0".into();
    let endpoints = c["clusters"][0]["endpoints"].as_array_mut().unwrap();
    assert_eq!(endpoints.len(), 3, "the drill has three upstreams");
    for (e, addr) in endpoints.iter_mut().zip(upstreams) {
        e["address"] = addr.as_str().into();
    }
    if tls {
        c["clusters"][0]["tls"] = serde_json::json!({
            "ca_file": fixture("ca.crt"),
            "server_name": "api.example.com",
            "cert_file": fixture("client.crt"),
            "key_file": fixture("client.key"),
        });
    }
    let dir = std::env::temp_dir().join(format!(
        "e2e-failover-{}-{name}{}",
        std::process::id(),
        if tls { "-tls" } else { "" }
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("proxy.json");
    std::fs::write(&path, serde_json::to_string_pretty(&c).unwrap()).unwrap();
    path
}

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../edgefront-core/tests/fixtures/tls")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// The upstreams' TLS: the `exact` certificate (`api.example.com`), and a
/// client certificate from the fixture CA required.
fn upstream_tls() -> std::sync::Arc<edgefront_core::tls::Acceptor> {
    use edgefront_core::config::{Certificate, ClientAuth, Tls, TlsVersion};
    use edgefront_core::tls::{Acceptor, Snapshot, Store};
    let tls = Tls {
        certificates: vec![Certificate {
            cert_file: fixture("exact.crt"),
            key_file: fixture("exact.key"),
            server_names: None,
        }],
        min_version: TlsVersion::Tls12,
        cipher_suites: None,
        handshake_timeout: Duration::from_secs(5),
        max_concurrent_handshakes: 1024,
        strict_sni: false,
        ocsp: Default::default(),
        client_auth: Some(ClientAuth {
            ca_file: fixture("ca.crt"),
            required: true,
        }),
        acme: None,
    };
    Acceptor::new(
        Store::new(std::sync::Arc::new(Snapshot::load(&tls).unwrap())),
        None,
    )
}

/// A running `edgefront proxy`, stopped with SIGTERM when dropped so it
/// writes its coverage profile, and killed if it does not stop.
struct Proc {
    child: Child,
    proxy: String,
    admin: String,
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

fn field(line: &str, name: &str) -> String {
    let key = format!("\"{name}\":\"");
    let at = line
        .find(&key)
        .unwrap_or_else(|| panic!("{name} in {line}"))
        + key.len();
    line[at..].split('"').next().unwrap().to_owned()
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
        // Keep reading after the listening line, so the proxy never blocks
        // on a full pipe.
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let _ = tx.send(line);
        }
    });
    let mut proc = Proc {
        child,
        proxy: String::new(),
        admin: String::new(),
    };
    let deadline = Instant::now() + TIMEOUT;
    let line = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match lines.recv_timeout(left) {
            Ok(line) if line.contains("\"listening\"") => break line,
            Ok(_) => {}
            Err(_) => panic!("the proxy did not log that it listens"),
        }
    };
    proc.proxy = field(&line, "proxy");
    proc.admin = field(&line, "admin");
    proc
}

/// One exchange on its own connection: the status and the body.
fn request(addr: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut conn = TcpStream::connect(addr).unwrap();
    conn.set_read_timeout(Some(TIMEOUT)).unwrap();
    write!(
        conn,
        "{method} {path} HTTP/1.1\r\nHost: drill\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = Vec::new();
    conn.read_to_end(&mut out).unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, body.to_owned())
}

/// Sets a fault through the upstream's control endpoint, as the drill's
/// `curl -X PUT .../state` does.
fn control(up: &Upstream, patch: &str) {
    let (status, body) = request(&up.control().to_string(), "PUT", "/state", patch);
    assert_eq!(status, 200, "{body}");
}

/// The drill's end state: `/clusters`' only cluster.
fn cluster(p: &Proc) -> serde_json::Value {
    let (status, body) = request(&p.admin, "GET", "/clusters", "");
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    v["clusters"][0].clone()
}

/// Runs the four phases against `config_name` and returns their reports
/// and the end state.
fn drill(config_name: &str, tls: bool) -> (Vec<Report>, serde_json::Value) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut ups: Vec<Upstream> = rt.block_on(async {
        let mut ups = Vec::new();
        for name in ["primary-a", "primary-b", "backup"] {
            let up = if tls {
                Upstream::start_tls(name, "127.0.0.1:0", "127.0.0.1:0", upstream_tls()).await
            } else {
                Upstream::start(name, "127.0.0.1:0", "127.0.0.1:0").await
            };
            ups.push(up.unwrap());
        }
        ups
    });
    let addrs = [0, 1, 2].map(|i| ups[i].addr().to_string());
    let p = start(&config(config_name, &addrs, tls));
    let deadline = Instant::now() + TIMEOUT;
    while request(&p.admin, "GET", "/readyz", "").0 != 200 {
        assert!(Instant::now() < deadline, "the proxy never became ready");
        thread::sleep(Duration::from_millis(50));
    }
    thread::sleep(Duration::from_secs(2)); // let the first health checks settle

    let secs = phase_seconds();
    let mut primary_a = Some(ups.remove(0));
    let mut reports = Vec::new();
    for phase in PHASES {
        let mut o = LoadOptions::new(format!("http://{}/", p.proxy).parse().unwrap());
        o.rate = RATE;
        o.duration = Duration::from_secs(secs);
        o.label = phase.to_owned();
        o.validate().unwrap();
        let running = rt.spawn(async move { load(&o).await });
        thread::sleep(Duration::from_secs(1)); // the fault lands while load is running
        match phase.as_bytes()[0] {
            b'2' => rt.block_on(primary_a.take().unwrap().kill()),
            b'3' => control(&ups[0], r#"{"error_rate":0.5}"#),
            b'4' => control(&ups[0], r#"{"blackhole":true}"#),
            _ => {}
        }
        let report = rt.block_on(running).unwrap();
        eprint!(
            "{config_name}{}: {}",
            if tls { " over https" } else { "" },
            report.row()
        );
        reports.push(report);
    }
    let end = cluster(&p);
    drop(p);
    drop(ups);
    (reports, end)
}

fn endpoint_flags(end: &serde_json::Value) -> Vec<(bool, bool)> {
    end["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["healthy"].as_bool().unwrap(),
                e["ejected"].as_bool().unwrap(),
            )
        })
        .collect()
}

#[test]
fn with_resilience_on_every_phase_succeeds_as_in_steers_table() {
    resilient(false);
}

#[test]
fn over_https_with_mutual_tls_every_phase_succeeds_too() {
    resilient(true);
}

fn resilient(tls: bool) {
    let (reports, end) = drill("resilient", tls);
    let requests = (RATE * phase_seconds() as f64) as u64;
    for r in &reports {
        assert_eq!(
            (r.requests, r.ok, r.success_percent),
            (requests, requests, 100.0),
            "{}: {:?}",
            r.label,
            r.failures
        );
        assert!(r.failures.is_empty(), "{}: {:?}", r.label, r.failures);
    }
    // Without a blackhole nothing waits on a timeout.
    for r in &reports[..3] {
        assert!(r.p99_ms < 100.0, "{}: p99 {} ms", r.label, r.p99_ms);
    }
    // The per-try timeout (250 ms) takes a request off the blackholed
    // primary long before the cluster timeout (1 s) would.
    assert!(
        reports[3].max_ms < 750.0,
        "blackhole: max {} ms",
        reports[3].max_ms
    );

    // Both primaries fail their health checks by the end, so priority 0
    // has nothing healthy and the backup takes all the load; primary-b was
    // also ejected for its 503s.
    assert_eq!(
        end["balancer"]["priority_load_percent"],
        serde_json::json!([0, 100]),
        "{end}"
    );
    let flags = endpoint_flags(&end);
    assert_eq!(
        (flags[0].0, flags[1].0, flags[2]),
        (false, false, (true, false)),
        "{end}"
    );
    assert!(
        end["endpoints"][1]["times_ejected"].as_u64().unwrap() >= 1,
        "{end}"
    );
}

#[test]
fn with_resilience_off_every_fault_reaches_the_client() {
    let (reports, end) = drill("plain", false);
    let requests = (RATE * phase_seconds() as f64) as u64;
    assert_eq!(reports[0].ok, requests, "{:?}", reports[0].failures);
    for r in &reports[1..] {
        assert_eq!(r.requests, requests);
        assert!(r.ok < requests, "{}: every request succeeded", r.label);
    }
    // Round robin keeps sending to the dead primary: connect errors.
    assert!(
        reports[1].failures.contains_key("http_502"),
        "{:?}",
        reports[1].failures
    );
    // The survivor's injected 503s are delivered as they are.
    assert!(
        reports[2].failures.contains_key("http_503"),
        "{:?}",
        reports[2].failures
    );
    // A blackholed request waits for the whole cluster timeout.
    assert!(
        reports[3].failures.contains_key("http_504"),
        "{:?}",
        reports[3].failures
    );
    assert!(reports[3].p99_ms >= 1000.0, "p99 {} ms", reports[3].p99_ms);
    // Nothing checks the endpoints, so the proxy still thinks all are fine.
    assert_eq!(endpoint_flags(&end), [(true, false); 3], "{end}");
}
