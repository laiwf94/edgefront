//! Discovery wired into the proxy: a watched file and listed endpoints
//! feeding a cluster's balancer while requests flow, readiness, a failing
//! source that keeps its last endpoints and says so in the metrics,
//! health checks that follow discovered endpoints without restarting, and
//! a reload that keeps discovery running.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use edgefront_core::config::Config;
use edgefront_core::listener::{ServerLimits, serve};
use edgefront_proxy::server::Proxy;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// A proxy listening on a free port with its checks and discovery
/// running, stopped when dropped.
struct Running {
    addr: SocketAddr,
    proxy: Arc<Proxy>,
    _stop: oneshot::Sender<()>,
    _stop_checks: oneshot::Sender<()>,
}

async fn start(config: &str) -> Running {
    let cfg = Config::from_json(config.as_bytes()).expect("a valid config");
    let proxy = Arc::new(Proxy::new(&cfg));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let (stop_checks, checks_stopped) = oneshot::channel::<()>();
    tokio::spawn(serve(
        listener,
        Arc::clone(&proxy),
        ServerLimits::default(),
        async move {
            let _ = stopped.await;
        },
    ));
    let checks = Arc::clone(&proxy);
    tokio::spawn(async move {
        checks
            .run_checks(async move {
                let _ = checks_stopped.await;
            })
            .await;
    });
    Running {
        addr,
        proxy,
        _stop: stop,
        _stop_checks: stop_checks,
    }
}

/// An upstream answering every request with its own name, recording each
/// request's path.
struct Upstream {
    addr: String,
    paths: Arc<Mutex<Vec<String>>>,
}

impl Upstream {
    fn served(&self, path: &str) -> usize {
        self.paths
            .lock()
            .unwrap()
            .iter()
            .filter(|p| *p == path)
            .count()
    }
}

async fn upstream(name: &'static str) -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let paths = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&paths);
    tokio::spawn(async move {
        loop {
            let Ok((conn, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let (r, mut w) = conn.into_split();
                let mut r = BufReader::new(r);
                loop {
                    let mut request_line = String::new();
                    if r.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let path = request_line.split(' ').nth(1).unwrap_or("").to_owned();
                    seen.lock().unwrap().push(path);
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{name}",
                        name.len()
                    );
                    if w.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    Upstream { addr, paths }
}

/// One request through the proxy: its status and body.
async fn get(proxy: SocketAddr) -> (u16, String) {
    let mut conn = TcpStream::connect(proxy).await.unwrap();
    conn.write_all(b"GET /work HTTP/1.1\r\nHost: web\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf = String::new();
    conn.read_to_string(&mut buf).await.unwrap();
    let status = buf[9..12].parse().unwrap();
    let body = buf.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned();
    (status, body)
}

/// The bodies of `n` requests, sorted and deduplicated.
async fn answers(proxy: SocketAddr, n: usize) -> Vec<String> {
    let mut seen = Vec::new();
    for _ in 0..n {
        let (status, body) = get(proxy).await;
        seen.push(if status == 200 {
            body
        } else {
            status.to_string()
        });
    }
    seen.sort();
    seen.dedup();
    seen
}

/// Waits until `n` requests are answered by exactly `want`.
async fn until_answers(proxy: SocketAddr, want: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let got = answers(proxy, 6).await;
        if got == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "answers stayed {got:?}, not {want:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "edgefront-discovery-proxy-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("endpoints.json")
}

/// Replaces the endpoint file by rename, as config tools do.
fn write(path: &Path, endpoints: &[(&str, bool)]) {
    let list: Vec<String> = endpoints
        .iter()
        .map(|(a, ready)| format!(r#"{{"address":"{a}","ready":{ready}}}"#))
        .collect();
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!(r#"{{"endpoints":[{}]}}"#, list.join(","))).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

fn config(file: &Path, listed: &[&str], extra: &str) -> String {
    let eps: Vec<String> = listed
        .iter()
        .map(|a| format!(r#"{{"address":"{a}"}}"#))
        .collect();
    format!(
        r#"{{"listen":"127.0.0.1:0","routes":[{{"path_prefix":"/","cluster":"web"}}],
            "clusters":[{{"name":"web","policy":"round_robin","endpoints":[{}],
            "discovery":{{"sources":[{{"file":"{}"}}]}}{extra}}}]}}"#,
        eps.join(","),
        file.display()
    )
}

fn metric(proxy: &Proxy, series: &str) -> Option<f64> {
    let text = proxy.metrics().registry().render();
    text.lines()
        .find_map(|l| l.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
}

#[tokio::test]
async fn a_watched_file_drives_traffic_as_it_changes() {
    let (a, b) = (upstream("a").await, upstream("b").await);
    let file = scratch("drives");
    let p = start(&config(&file, &[], "")).await;
    // Nothing discovered yet: no endpoint to send to.
    assert_eq!(get(p.addr).await.0, 503);
    write(&file, &[(&a.addr, true)]);
    until_answers(p.addr, &["a"]).await;
    write(&file, &[(&a.addr, true), (&b.addr, true)]);
    until_answers(p.addr, &["a", "b"]).await;
    write(&file, &[(&b.addr, true)]);
    until_answers(p.addr, &["b"]).await;
    let series = r#"edgefront_discovery_endpoints{cluster="web"}"#;
    assert_eq!(metric(&p.proxy, series), Some(1.0));
    assert_eq!(p.proxy.endpoints("web").unwrap().len(), 1);
    write(&file, &[]);
    until_answers(p.addr, &["503"]).await;
}

#[tokio::test]
async fn listed_endpoints_serve_until_discovery_answers_and_lose_to_it() {
    let (a, b) = (upstream("a").await, upstream("b").await);
    let file = scratch("listed");
    let p = start(&config(&file, &[&a.addr], "")).await;
    until_answers(p.addr, &["a"]).await;
    // The file names a too, not ready: the file is listed first, so it
    // wins a, and b joins.
    write(&file, &[(&a.addr, false), (&b.addr, true)]);
    until_answers(p.addr, &["b"]).await;
    let found = p.proxy.discovered_endpoints("web").unwrap();
    let flags: Vec<(String, bool)> = found.iter().map(|e| (e.address.clone(), e.ready)).collect();
    let mut want = vec![(a.addr.clone(), false), (b.addr.clone(), true)];
    want.sort();
    assert_eq!(flags, want);
    // a turns ready: a readiness flip, not a removal.
    write(&file, &[(&a.addr, true), (&b.addr, true)]);
    until_answers(p.addr, &["a", "b"]).await;
}

#[tokio::test]
async fn a_failing_source_keeps_its_endpoints_and_says_so() {
    let a = upstream("a").await;
    let file = scratch("failing");
    let p = start(&config(&file, &[], "")).await;
    write(&file, &[(&a.addr, true)]);
    until_answers(p.addr, &["a"]).await;
    let source = format!(r#"cluster="web",source="file:{}""#, file.display());
    let stale = format!("edgefront_discovery_stale{{{source}}}");
    let failures = format!("edgefront_discovery_failures_total{{{source}}}");
    assert_eq!(metric(&p.proxy, &stale), Some(0.0));
    std::fs::write(&file, "{\"endpoints\": [").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while metric(&p.proxy, &stale) != Some(1.0) {
        assert!(Instant::now() < deadline, "the failure never showed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(metric(&p.proxy, &failures).unwrap() >= 1.0);
    assert_eq!(answers(p.addr, 6).await, ["a"]);
    std::fs::remove_file(&file).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(answers(p.addr, 6).await, ["a"]);
    assert!(metric(&p.proxy, &failures).unwrap() >= 2.0);
    write(&file, &[(&a.addr, true)]);
    while metric(&p.proxy, &stale) != Some(0.0) {
        assert!(Instant::now() < deadline, "the recovery never showed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn health_checks_follow_discovered_endpoints_without_restarting() {
    let (a, b) = (upstream("a").await, upstream("b").await);
    let file = scratch("health");
    let check = r#","health_check":{"path":"/healthz","interval":"1s","jitter":"0s","timeout":"1s",
                  "healthy_threshold":1,"unhealthy_threshold":1}"#;
    let p = start(&config(&file, &[], check)).await;
    write(&file, &[(&a.addr, true)]);
    until_answers(p.addr, &["a"]).await;
    // b comes and goes 20 times in 1.2 s. a stays throughout, so it keeps
    // its one-second schedule: about two probes, not one per change.
    let began = Instant::now();
    for i in 0..20 {
        if i % 2 == 0 {
            write(&file, &[(&a.addr, true), (&b.addr, true)]);
        } else {
            write(&file, &[(&a.addr, true)]);
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    let probes = a.served("/healthz");
    let took = began.elapsed().as_secs_f64();
    assert!(
        probes as f64 <= took + 2.0,
        "a was probed {probes} times in {took:.1} s"
    );
    // b was probed when it arrived.
    assert!(b.served("/healthz") >= 1);
}

#[tokio::test]
async fn a_reload_with_the_same_sources_keeps_discovery_running() {
    let a = upstream("a").await;
    let file = scratch("reload");
    let text = config(&file, &[], "");
    let p = start(&text).await;
    write(&file, &[(&a.addr, true)]);
    until_answers(p.addr, &["a"]).await;
    let cfg = Config::from_json(text.as_bytes()).unwrap();
    assert_eq!(p.proxy.reload(&cfg).unwrap(), 2);
    // Served at once, from the discovered set, with no gap.
    assert_eq!(answers(p.addr, 6).await, ["a"]);
    let applies = r#"edgefront_discovery_apply_duration_seconds_count{cluster="web"}"#;
    let before = metric(&p.proxy, applies).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(metric(&p.proxy, applies).unwrap(), before);
}
