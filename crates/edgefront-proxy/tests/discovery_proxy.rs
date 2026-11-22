//! Discovery wired into the proxy: a watched file and listed endpoints
//! feeding a cluster's balancer while requests flow, readiness, a failing
//! source that keeps its last endpoints and says so in the metrics,
//! health checks that follow discovered endpoints without restarting, a
//! reload that keeps discovery running, and a delta feed's churn patching
//! the cluster in place under traffic for every policy.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use edgefront_core::config::Config;
use edgefront_core::listener::{ServerLimits, serve};
use edgefront_proxy::discovery::Endpoint as DiscEndpoint;
use edgefront_proxy::discovery::delta::{Feed, serve as serve_feed};
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

/// The ready endpoints of `feed` as the balancer should hold them.
fn ready_set(feed: &Feed) -> Vec<(String, u32, u32)> {
    let mut want: Vec<_> = feed
        .endpoints()
        .into_iter()
        .filter(|e| e.ready)
        .map(|e| (e.address, e.weight, u32::from(e.priority)))
        .collect();
    want.sort();
    want
}

fn balanced(proxy: &Proxy) -> Vec<(String, u32, u32)> {
    let mut got: Vec<_> = proxy
        .endpoints("web")
        .unwrap()
        .into_iter()
        .map(|e| (e.address.to_string(), e.weight, e.priority))
        .collect();
    got.sort();
    got
}

/// A delta feed's churn patches the cluster in place rather than
/// rebuilding it: after hundreds of random upserts, removals, weight and
/// priority changes and readiness flips, for each policy, the balancer
/// holds exactly the feed's ready endpoints, the endpoints that stayed
/// throughout keep the same state, the attempt series of removed
/// endpoints are gone while those of every endpoint balanced over are
/// there, and every request sent meanwhile is answered by an upstream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delta_feed_patches_the_cluster_in_place_under_traffic() {
    let ups = [
        upstream("a").await,
        upstream("b").await,
        upstream("c").await,
    ];
    for policy in ["round_robin", "least_loaded", "maglev"] {
        let feed = Feed::new(1 << 16);
        for u in &ups {
            feed.upsert(DiscEndpoint::new(u.addr.clone()));
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let feed_addr = listener.local_addr().unwrap();
        let (stop_feed, feed_stopped) = oneshot::channel::<()>();
        tokio::spawn(serve_feed(
            listener,
            Arc::clone(&feed),
            Duration::from_millis(200),
            async move {
                let _ = feed_stopped.await;
            },
        ));
        let hash = if policy == "maglev" {
            r#""hash_header":"x-user","#
        } else {
            ""
        };
        let p = start(&format!(
            r#"{{"listen":"127.0.0.1:0","routes":[{{"path_prefix":"/","cluster":"web"}}],
                "clusters":[{{"name":"web","policy":"{policy}",{hash}
                "discovery":{{"sources":[{{"delta":{{"url":"http://{feed_addr}/v1/web"}}}}]}}}}]}}"#
        ))
        .await;
        // Least loaded keeps away from the slowest of three for a while,
        // so wait for the set rather than for every name to answer.
        let deadline = Instant::now() + Duration::from_secs(5);
        while balanced(&p.proxy) != ready_set(&feed) {
            assert!(
                Instant::now() < deadline,
                "{policy}: the first set never came"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let kept: Vec<_> = p.proxy.endpoints("web").unwrap();
        // Traffic throughout: the upstreams stay ready at priority 0, so
        // the churn, all at priority 1 and up, never takes a request.
        let traffic = {
            let addr = p.addr;
            tokio::spawn(async move {
                let mut statuses = Vec::new();
                for _ in 0..150 {
                    statuses.push(get(addr).await.0);
                }
                statuses
            })
        };
        let rng = edgefront_proxy::balancer::rng::SplitMix64::new(42);
        use edgefront_proxy::balancer::rng::Rng;
        let fake = |i: usize| format!("10.77.{}.{}:80", i / 250, i % 250 + 1);
        let mut next = 0;
        let mut live: Vec<String> = Vec::new();
        for round in 0..40 {
            for _ in 0..1 + rng.below(30) {
                match rng.below(5) {
                    0 | 1 => {
                        live.push(fake(next));
                        next += 1;
                        let e = DiscEndpoint {
                            priority: 1 + rng.below(3) as u8,
                            weight: 1 + rng.below(9) as u32,
                            ..DiscEndpoint::new(live.last().unwrap().clone())
                        };
                        feed.upsert(e);
                    }
                    2 if !live.is_empty() => {
                        let gone = live.swap_remove(rng.below(live.len()));
                        feed.remove(&gone);
                    }
                    3 if !live.is_empty() => {
                        let e = DiscEndpoint {
                            priority: 1 + rng.below(3) as u8,
                            weight: 1 + rng.below(9) as u32,
                            ready: rng.below(3) != 0,
                            ..DiscEndpoint::new(live[rng.below(live.len())].clone())
                        };
                        feed.upsert(e);
                    }
                    _ => {
                        // The upstreams' weights move too.
                        let e = DiscEndpoint {
                            weight: 1 + rng.below(5) as u32,
                            ..DiscEndpoint::new(ups[rng.below(3)].addr.clone())
                        };
                        feed.upsert(e);
                    }
                }
            }
            let want = ready_set(&feed);
            let deadline = Instant::now() + Duration::from_secs(5);
            while balanced(&p.proxy) != want {
                assert!(
                    Instant::now() < deadline,
                    "{policy} round {round}: the balancer never matched the feed"
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            // The discovered set, ready or not, is the feed's.
            let found = p.proxy.discovered_endpoints("web").unwrap();
            assert_eq!(found, feed.endpoints(), "{policy} round {round}");
        }
        let statuses = traffic.await.unwrap();
        assert!(statuses.iter().all(|&s| s == 200), "{policy}: {statuses:?}");
        // The upstreams kept their state throughout.
        let now = p.proxy.endpoints("web").unwrap();
        for k in &kept {
            let e = now.iter().find(|e| e.address == k.address).unwrap();
            assert!(
                Arc::ptr_eq(&e.state, &k.state),
                "{policy}: {} lost its state",
                k.address
            );
        }
        // Attempt series for exactly the endpoints balanced over.
        let text = p.proxy.metrics().registry().render();
        for i in 0..next {
            let series = format!(r#"endpoint="{}""#, fake(i));
            let balanced = now.iter().any(|e| *e.address == fake(i));
            assert_eq!(text.contains(&series), balanced, "{policy}: {}", fake(i));
        }
        let _ = stop_feed.send(());
    }
}
