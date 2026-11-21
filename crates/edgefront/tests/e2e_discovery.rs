//! End to end: discovery driving live traffic through the `edgefront
//! proxy` binary while endpoints change.
//!
//! Each test starts real upstreams that answer with their own name, a
//! discovery source the test controls, and the binary with one cluster
//! fed by that source. Load runs through the proxy the whole time, and
//! the upstreams record when each request reached them, so the tests can
//! say when an endpoint started and stopped taking traffic.
//!
//! - **DNS (A records).** A name's records gain an address and lose one
//!   while load runs. The new endpoint takes traffic, and the removed one
//!   takes none from one TTL plus one second after its record went (the
//!   W07 Done-when bound). A DNS outage keeps the last addresses serving,
//!   with the failure and staleness in `/metrics`, until DNS answers again.
//! - **DNS (SRV).** Weights split the load 3 to 1, the lower priority waits
//!   as a backup, and removing the preferred records moves all the load to
//!   it.
//! - **A watched file.** Endpoints are added, made not ready and removed by
//!   atomic rename; a broken file keeps the last set serving and shows in
//!   `/metrics`.
//! - **A delta feed.** An HTTP/2 feed in this process adds and removes
//!   endpoints, is killed mid-stream while changes keep coming, and is
//!   restarted on the same port. The proxy keeps serving the last set while
//!   the feed is down, then resumes from its token (one snapshot in all)
//!   and lands on exactly the feed's set.
//!
//! Load starts once the source's first answer has reached the balancer
//! (until then the cluster has no endpoints and answers 503), and from then
//! on every request in every test must be answered 200: an endpoint coming
//! or going, or a source failing, never reaches a client as an error. The DNS tests bind upstreams
//! on 127.0.0.2 to 127.0.0.4, which Linux routes to loopback.

#[path = "../../edgefront-proxy/tests/common/dns_server.rs"]
mod dns_server;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use dns_server::DnsServer;
use edgefront_proxy::discovery::Endpoint;
use edgefront_proxy::discovery::delta::{Feed, serve};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const EDGEFRONT: &str = env!("CARGO_BIN_EXE_edgefront");
const TIMEOUT: Duration = Duration::from_secs(10);
/// The TTL of every DNS record the tests serve.
const TTL: u32 = 1;
/// The Done-when bound: an endpoint removed from DNS takes no traffic once
/// one TTL plus one second has passed.
const REMOVAL_BOUND: Duration = Duration::from_secs(TTL as u64 + 1);

// ---------------------------------------------------------------------------
// Upstreams

/// An HTTP/1.1 upstream that answers every request with its name and
/// records when each request reached it.
struct Upstream {
    name: &'static str,
    addr: SocketAddr,
    arrivals: Arc<Mutex<Vec<Instant>>>,
    task: JoinHandle<()>,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Upstream {
    async fn start(name: &'static str, bind: &str) -> std::io::Result<Upstream> {
        let listener = TcpListener::bind(bind).await?;
        let addr = listener.local_addr()?;
        let arrivals = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&arrivals);
        let task = tokio::spawn(async move {
            loop {
                let Ok((conn, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(answer(conn, name, Arc::clone(&log)));
            }
        });
        Ok(Upstream {
            name,
            addr,
            arrivals,
            task,
        })
    }

    /// How many requests reached it.
    fn served(&self) -> usize {
        self.arrivals.lock().unwrap().len()
    }

    /// How many requests reached it at or after `t`.
    fn served_since(&self, t: Instant) -> usize {
        self.arrivals
            .lock()
            .unwrap()
            .iter()
            .filter(|a| **a >= t)
            .count()
    }

    /// When the latest request reached it.
    fn last(&self) -> Option<Instant> {
        self.arrivals.lock().unwrap().last().copied()
    }
}

async fn answer(conn: tokio::net::TcpStream, name: &'static str, log: Arc<Mutex<Vec<Instant>>>) {
    let (r, mut w) = conn.into_split();
    let mut r = tokio::io::BufReader::new(r);
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        loop {
            let mut header = String::new();
            if r.read_line(&mut header).await.unwrap_or(0) == 0 {
                return;
            }
            if header == "\r\n" {
                break;
            }
        }
        log.lock().unwrap().push(Instant::now());
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{name}",
            name.len()
        );
        if w.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// Upstreams with the given names on 127.0.0.2, 127.0.0.3 and so on, all
/// on one port, as the endpoints of one DNS name are.
async fn on_one_port(names: &[&'static str]) -> Vec<Upstream> {
    'port: for _ in 0..50 {
        let first = Upstream::start(names[0], "127.0.0.2:0").await.unwrap();
        let port = first.addr.port();
        let mut ups = vec![first];
        for (i, name) in names.iter().enumerate().skip(1) {
            match Upstream::start(name, &format!("127.0.0.{}:{port}", i + 2)).await {
                Ok(up) => ups.push(up),
                Err(_) => continue 'port,
            }
        }
        return ups;
    }
    panic!("no port free on every loopback address");
}

// ---------------------------------------------------------------------------
// The proxy

/// A running `edgefront proxy`, stopped with SIGTERM when dropped so it
/// writes its coverage profile, and killed if it does not stop.
struct Proc {
    child: Child,
    proxy: String,
    admin: String,
    _dir: Scratch,
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

/// A scratch directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "edgefront-e2e-discovery-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
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

/// Starts the binary with one round-robin cluster `web`, fed by
/// `discovery`, behind a catch-all route, and waits until it is ready and
/// discovery has given the cluster its first endpoints.
fn start(name: &str, discovery: serde_json::Value) -> Proc {
    let dir = Scratch::new(name);
    let config = serde_json::json!({
        "listen": "127.0.0.1:0",
        "admin_listen": "127.0.0.1:0",
        "routes": [{"path_prefix": "/", "cluster": "web"}],
        "clusters": [{
            "name": "web",
            "policy": "round_robin",
            "endpoints": [],
            "connect_timeout": "250ms",
            "timeout": "2s",
            "discovery": discovery,
        }],
    });
    let path = dir.0.join("proxy.json");
    std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    let mut child = Command::new(EDGEFRONT)
        .args(["proxy", "--config"])
        .arg(&path)
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
        _dir: dir,
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
    while request(&proc.admin, "/readyz").0 != 200 {
        assert!(Instant::now() < deadline, "the proxy never became ready");
        thread::sleep(Duration::from_millis(20));
    }
    // Ready is not yet served: the cluster lists no endpoints, so it
    // answers 503 until the source's first answer reaches the balancer.
    while request(&proc.proxy, "/work").0 != 200 {
        assert!(
            Instant::now() < deadline,
            "discovery never gave the cluster an endpoint"
        );
        thread::sleep(Duration::from_millis(10));
    }
    proc
}

/// One GET on its own connection: the status and the body.
fn request(addr: &str, path: &str) -> (u16, String) {
    let Ok(mut conn) = TcpStream::connect(addr) else {
        return (0, String::new());
    };
    conn.set_read_timeout(Some(TIMEOUT)).unwrap();
    write!(
        conn,
        "GET {path} HTTP/1.1\r\nHost: web\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = Vec::new();
    if conn.read_to_end(&mut out).is_err() {
        return (0, String::new());
    }
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, body.to_owned())
}

/// The value of one series in the proxy's `/metrics`.
fn metric(p: &Proc, series: &str) -> Option<f64> {
    let (status, text) = request(&p.admin, "/metrics");
    assert_eq!(status, 200, "{text}");
    text.lines()
        .find_map(|l| l.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
}

/// Waits until `series` reads `want`.
async fn until_metric(p: &Proc, series: &str, want: f64, within: Duration) {
    let deadline = Instant::now() + within;
    loop {
        let got = metric(p, series);
        if got == Some(want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{series} stayed {got:?}, not {want}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Load

/// Requests sent through the proxy back to back by a few clients until
/// stopped, each answer kept with when it came back.
struct Load {
    stop: Arc<AtomicBool>,
    answers: Arc<Mutex<Vec<(Instant, u16, String)>>>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl Load {
    fn start(proxy: &str) -> Load {
        let stop = Arc::new(AtomicBool::new(false));
        let answers = Arc::new(Mutex::new(Vec::new()));
        let workers = (0..4)
            .map(|_| {
                let (stop, answers, proxy) =
                    (Arc::clone(&stop), Arc::clone(&answers), proxy.to_owned());
                thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let (status, body) = request(&proxy, "/work");
                        answers.lock().unwrap().push((Instant::now(), status, body));
                        thread::sleep(Duration::from_millis(2));
                    }
                })
            })
            .collect();
        Load {
            stop,
            answers,
            workers,
        }
    }

    /// The answers that came back since `t`, counted by body (`503`, say,
    /// for an answer that was not a 200).
    fn since(&self, t: Instant) -> BTreeMap<String, usize> {
        let mut by = BTreeMap::new();
        for (at, status, body) in self.answers.lock().unwrap().iter() {
            if *at >= t {
                let key = if *status == 200 {
                    body.clone()
                } else {
                    status.to_string()
                };
                *by.entry(key).or_default() += 1;
            }
        }
        by
    }

    /// Stops the clients and checks that every answer was a 200; returns
    /// how many there were.
    fn finish(mut self) -> usize {
        self.stop.store(true, Ordering::Relaxed);
        for w in self.workers.drain(..) {
            w.join().unwrap();
        }
        let answers = self.answers.lock().unwrap();
        let failed: Vec<_> = answers.iter().filter(|(_, s, _)| *s != 200).collect();
        assert!(
            failed.is_empty(),
            "{} of {} requests failed, the first: {:?}",
            failed.len(),
            answers.len(),
            failed.first()
        );
        answers.len()
    }
}

/// Waits until `ups` are the upstreams answering: each of them has served
/// a request since the wait began, and nothing else has since `quiet`
/// before the end. Returns how long it took for the last of them to start.
async fn until_serving(load: &Load, ups: &[&Upstream], others: &[&Upstream]) -> Duration {
    let began = Instant::now();
    let deadline = began + TIMEOUT;
    let mut started = None;
    loop {
        let since = load.since(began);
        let all = ups.iter().all(|u| since.contains_key(u.name));
        if all && started.is_none() {
            started = Some(began.elapsed());
        }
        // Then 300 ms in which only `ups` answer.
        if let Some(s) = started {
            let window = began + s;
            let quiet = Instant::now() - Duration::from_millis(300);
            if quiet >= window {
                let recent = load.since(quiet);
                if others.iter().all(|o| !recent.contains_key(o.name))
                    && ups.iter().all(|u| recent.contains_key(u.name))
                {
                    return s;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "never served by just {:?}: {:?} since the wait began",
            ups.iter().map(|u| u.name).collect::<Vec<_>>(),
            load.since(began)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Checks that `up` took no request from `bound` after `removed`, and
/// none in the second after that; returns when it took its last one.
async fn stopped_within(up: &Upstream, removed: Instant, bound: Duration) -> Duration {
    tokio::time::sleep_until((removed + bound + Duration::from_secs(1)).into()).await;
    let last = up.last().expect("it served before its removal");
    let after = last.saturating_duration_since(removed);
    assert!(
        after <= bound,
        "{} took a request {after:?} after its removal, past {bound:?}",
        up.name
    );
    assert_eq!(up.served_since(removed + bound), 0, "{}", up.name);
    after
}

// ---------------------------------------------------------------------------
// DNS

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dns_records_move_live_traffic_and_an_outage_keeps_the_last_addresses() {
    let dns = DnsServer::start().await;
    let ups = on_one_port(&["a", "b", "c"]).await;
    let (a, b, c) = (&ups[0], &ups[1], &ups[2]);
    let port = a.addr.port();
    let ip = |u: &Upstream| u.addr.ip().to_string();
    dns.set_ips("api.test", &[&ip(a), &ip(b)], TTL);

    let p = start(
        "dns",
        serde_json::json!({
            "sources": [{"dns": format!("api.test:{port}")}],
            "dns_servers": [dns.addr.to_string()],
        }),
    );
    let load = Load::start(&p.proxy);
    until_serving(&load, &[a, b], &[c]).await;

    // c's record appears: it takes traffic by the next lookup, one TTL.
    dns.set_ips("api.test", &[&ip(a), &ip(b), &ip(c)], TTL);
    let joined = until_serving(&load, &[a, b, c], &[]).await;
    assert!(joined <= REMOVAL_BOUND, "c joined after {joined:?}");

    // a's record goes: a takes nothing from one TTL plus one second on.
    let removed = Instant::now();
    dns.set_ips("api.test", &[&ip(b), &ip(c)], TTL);
    let last = stopped_within(a, removed, REMOVAL_BOUND).await;
    eprintln!("dns: c joined after {joined:?}; a's last request {last:?} after its removal");
    let source = format!(r#"cluster="web",source="dns:api.test:{port}""#);
    assert_eq!(
        metric(&p, r#"edgefront_discovery_endpoints{cluster="web"}"#),
        Some(2.0)
    );

    // DNS stops answering: b and c keep serving, and the failure shows.
    dns.silence(true);
    let silent = Instant::now();
    let stale = format!("edgefront_discovery_stale{{{source}}}");
    until_metric(&p, &stale, 1.0, TIMEOUT).await;
    assert!(
        metric(
            &p,
            &format!("edgefront_discovery_failures_total{{{source}}}")
        )
        .unwrap()
            >= 1.0
    );
    // Past several TTLs without an answer, still the same two.
    tokio::time::sleep(Duration::from_secs(3 * TTL as u64)).await;
    let during = load.since(silent);
    assert_eq!(
        during.keys().collect::<Vec<_>>(),
        ["b", "c"],
        "during the outage: {during:?}"
    );

    // DNS answers again, now without b: fresh, and b goes.
    dns.set_ips("api.test", &[&ip(c)], TTL);
    dns.silence(false);
    until_metric(&p, &stale, 0.0, Duration::from_secs(15)).await;
    until_serving(&load, &[c], &[a, b]).await;
    let answered = load.finish();
    eprintln!("dns: {answered} requests, every one 200");
    assert!(answered >= 500, "only {answered} requests");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn srv_weights_split_the_load_and_a_lower_priority_takes_over() {
    let dns = DnsServer::start().await;
    let heavy = Upstream::start("heavy", "127.0.0.1:0").await.unwrap();
    let light = Upstream::start("light", "127.0.0.1:0").await.unwrap();
    let backup = Upstream::start("backup", "127.0.0.1:0").await.unwrap();
    dns.set_ips("pool.test", &["127.0.0.1"], TTL);
    let records = |with_primaries: bool| {
        let mut r = vec![(20, 1, backup.addr.port(), "pool.test")];
        if with_primaries {
            r.push((10, 30, heavy.addr.port(), "pool.test"));
            r.push((10, 10, light.addr.port(), "pool.test"));
        }
        r
    };
    dns.set_srv("_http._tcp.web.test", &records(true), TTL);

    let p = start(
        "srv",
        serde_json::json!({
            "sources": [{"srv": "_http._tcp.web.test"}],
            "dns_servers": [dns.addr.to_string()],
        }),
    );
    let load = Load::start(&p.proxy);
    until_serving(&load, &[&heavy, &light], &[&backup]).await;

    // Over a second of load, 3 to 1, and nothing to the backup.
    let from = (heavy.served(), light.served());
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (h, l) = (heavy.served() - from.0, light.served() - from.1);
    let ratio = h as f64 / l as f64;
    eprintln!("srv: heavy {h}, light {l}, ratio {ratio:.2}");
    assert!((2.5..=3.5).contains(&ratio), "heavy {h}, light {l}");
    assert_eq!(backup.served(), 0);

    // The priority-10 records go: everything moves to the backup.
    let removed = Instant::now();
    dns.set_srv("_http._tcp.web.test", &records(false), TTL);
    until_serving(&load, &[&backup], &[&heavy, &light]).await;
    stopped_within(&heavy, removed, REMOVAL_BOUND).await;
    stopped_within(&light, removed, REMOVAL_BOUND).await;
    load.finish();
}

// ---------------------------------------------------------------------------
// A watched file

/// Replaces the endpoint file by rename, as config tools do.
fn write_endpoints(path: &Path, endpoints: &[(&Upstream, bool)]) {
    let list: Vec<serde_json::Value> = endpoints
        .iter()
        .map(|(u, ready)| serde_json::json!({"address": u.addr.to_string(), "ready": ready}))
        .collect();
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::json!({ "endpoints": list }).to_string()).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_watched_file_moves_live_traffic_and_a_broken_one_keeps_the_last_set() {
    let a = Upstream::start("a", "127.0.0.1:0").await.unwrap();
    let b = Upstream::start("b", "127.0.0.1:0").await.unwrap();
    let c = Upstream::start("c", "127.0.0.1:0").await.unwrap();
    let files = Scratch::new("file-endpoints");
    let file = files.0.join("endpoints.json");
    write_endpoints(&file, &[(&a, true), (&b, true)]);

    let p = start(
        "file",
        serde_json::json!({"sources": [{"file": file.to_string_lossy()}]}),
    );
    let load = Load::start(&p.proxy);
    until_serving(&load, &[&a, &b], &[&c]).await;

    write_endpoints(&file, &[(&a, true), (&b, true), (&c, true)]);
    until_serving(&load, &[&a, &b, &c], &[]).await;

    // b fails its readiness: out of the balancer, still discovered.
    write_endpoints(&file, &[(&a, true), (&b, false), (&c, true)]);
    until_serving(&load, &[&a, &c], &[&b]).await;
    assert_eq!(
        metric(&p, r#"edgefront_discovery_endpoints{cluster="web"}"#),
        Some(3.0)
    );

    write_endpoints(&file, &[(&b, false), (&c, true)]);
    until_serving(&load, &[&c], &[&a, &b]).await;

    // A half-written file, then none at all: c keeps serving.
    let source = format!(r#"cluster="web",source="file:{}""#, file.display());
    let stale = format!("edgefront_discovery_stale{{{source}}}");
    std::fs::write(&file, r#"{"endpoints": ["#).unwrap();
    until_metric(&p, &stale, 1.0, TIMEOUT).await;
    std::fs::remove_file(&file).unwrap();
    let broken = Instant::now();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let during = load.since(broken);
    assert_eq!(during.keys().collect::<Vec<_>>(), ["c"], "{during:?}");
    assert!(
        metric(
            &p,
            &format!("edgefront_discovery_failures_total{{{source}}}")
        )
        .unwrap()
            >= 2.0
    );

    // A good file again: fresh, and its set serves.
    write_endpoints(&file, &[(&a, true), (&b, true)]);
    until_metric(&p, &stale, 0.0, TIMEOUT).await;
    until_serving(&load, &[&a, &b], &[&c]).await;
    let answered = load.finish();
    eprintln!("file: {answered} requests, every one 200");
}

// ---------------------------------------------------------------------------
// A delta feed

/// Serves `feed` on `addr` (a free port when `None`) with a 200 ms
/// heartbeat, until the handle is aborted, which drops every stream.
async fn feed_on(feed: &Arc<Feed>, addr: Option<SocketAddr>) -> (SocketAddr, JoinHandle<()>) {
    let deadline = Instant::now() + TIMEOUT;
    let listener = loop {
        match TcpListener::bind(addr.unwrap_or_else(|| "127.0.0.1:0".parse().unwrap())).await {
            Ok(l) => break l,
            // The old listener's port can take a moment to come free.
            Err(e) => {
                assert!(Instant::now() < deadline, "rebinding the feed: {e}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    };
    let at = listener.local_addr().unwrap();
    let task = tokio::spawn(serve(
        listener,
        Arc::clone(feed),
        Duration::from_millis(200),
        std::future::pending(),
    ));
    (at, task)
}

fn ep(u: &Upstream) -> Endpoint {
    Endpoint::new(u.addr.to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delta_feed_moves_live_traffic_and_is_resumed_after_a_kill() {
    let a = Upstream::start("a", "127.0.0.1:0").await.unwrap();
    let b = Upstream::start("b", "127.0.0.1:0").await.unwrap();
    let c = Upstream::start("c", "127.0.0.1:0").await.unwrap();
    let d = Upstream::start("d", "127.0.0.1:0").await.unwrap();
    let feed = Feed::new(1024);
    feed.upsert(ep(&a));
    feed.upsert(ep(&b));
    let (addr, task) = feed_on(&feed, None).await;
    let url = format!("http://{addr}/v1/clusters/web");

    let p = start(
        "delta",
        serde_json::json!({"sources": [{"delta": {"url": url}}]}),
    );
    let load = Load::start(&p.proxy);
    until_serving(&load, &[&a, &b], &[&c, &d]).await;

    // Changes, one endpoint at a time.
    feed.upsert(ep(&c));
    until_serving(&load, &[&a, &b, &c], &[&d]).await;
    feed.remove(&a.addr.to_string());
    until_serving(&load, &[&b, &c], &[&a, &d]).await;

    // The feed dies mid-stream: b and c keep serving, and the failure
    // shows.
    task.abort();
    let killed = Instant::now();
    let source = format!(r#"cluster="web",source="delta:{url}""#);
    let stale = format!("edgefront_discovery_stale{{{source}}}");
    until_metric(&p, &stale, 1.0, TIMEOUT).await;
    // Changes keep coming while it is down.
    feed.remove(&b.addr.to_string());
    feed.upsert(ep(&d));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let during = load.since(killed);
    assert_eq!(
        during.keys().collect::<Vec<_>>(),
        ["b", "c"],
        "while the feed was down: {during:?}"
    );
    assert!(
        metric(
            &p,
            &format!("edgefront_discovery_failures_total{{{source}}}")
        )
        .unwrap()
            >= 1.0
    );

    // Back on the same port: the proxy resumes from its token, without a
    // second snapshot, and serves exactly the feed's set.
    let (_, _task) = feed_on(&feed, Some(addr)).await;
    let restarted = Instant::now();
    until_serving(&load, &[&c, &d], &[&a, &b]).await;
    until_metric(&p, &stale, 0.0, TIMEOUT).await;
    let (snapshots, resumes) = feed.starts();
    assert_eq!(snapshots, 1, "one snapshot in all");
    assert!(resumes >= 1, "the proxy resumed");
    eprintln!(
        "delta: back in {:?} after the restart; {snapshots} snapshot, {resumes} resume(s)",
        restarted.elapsed()
    );
    assert_eq!(
        metric(&p, r#"edgefront_discovery_endpoints{cluster="web"}"#),
        Some(2.0)
    );
    let answered = load.finish();
    eprintln!("delta: {answered} requests, every one 200");
}
