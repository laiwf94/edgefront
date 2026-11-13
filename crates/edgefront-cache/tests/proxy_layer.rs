//! CacheLayer on edgefront-proxy routes, over real TCP: one cached route
//! and one that is not, in front of a counting upstream.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use edgefront_cache::config::CacheConfig;
use edgefront_cache::layer::{CacheLayer, Clock, LayerSettings};
use edgefront_cache::store::{Cache, CacheSettings};
use edgefront_core::config::Config;
use edgefront_core::listener::{ServerLimits, serve};
use edgefront_proxy::server::Proxy;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

struct TestClock(Mutex<SystemTime>);

impl TestClock {
    fn advance(&self, secs: u64) {
        *self.0.lock().unwrap() += Duration::from_secs(secs);
    }
}

impl Clock for TestClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap()
    }
}

/// An upstream that counts requests and answers each with `reply` after
/// `delay`, or closes the connection unanswered while `down` is set.
struct Upstream {
    addr: String,
    requests: Arc<AtomicUsize>,
    down: Arc<AtomicBool>,
}

async fn upstream(reply: &'static str, delay: Duration) -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let requests = Arc::new(AtomicUsize::new(0));
    let down = Arc::new(AtomicBool::new(false));
    let (n, d) = (requests.clone(), down.clone());
    tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            let (n, d) = (n.clone(), d.clone());
            tokio::spawn(async move {
                let (r, mut w) = conn.into_split();
                let mut r = BufReader::new(r);
                loop {
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    n.fetch_add(1, Ordering::SeqCst);
                    if d.load(Ordering::SeqCst) {
                        return;
                    }
                    tokio::time::sleep(delay).await;
                    if w.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    Upstream {
        addr,
        requests,
        down,
    }
}

struct Running {
    addr: SocketAddr,
    clock: Arc<TestClock>,
    _stop: oneshot::Sender<()>,
}

/// The proxy with the cache on route 0 (`a`, `/`) and not on route 1
/// (`a`, `/plain/`).
async fn start(up: &Upstream) -> Running {
    let config = format!(
        "{{\"listen\":\"127.0.0.1:0\",\"routes\":[\
         {{\"host\":\"a\",\"path_prefix\":\"/\",\"cluster\":\"web\"}},\
         {{\"host\":\"a\",\"path_prefix\":\"/plain/\",\"cluster\":\"web\"}}],\
         \"clusters\":[{{\"name\":\"web\",\"endpoints\":[{{\"address\":\"{}\"}}]}}]}}",
        up.addr
    );
    let cfg = Config::from_json(config.as_bytes()).expect("a valid config");
    let cache_cfg: CacheConfig =
        toml::from_str("routes = [{ host = \"a\", path_prefix = \"/\" }]").unwrap();
    let policies = cache_cfg.resolve(&cfg.routes).unwrap();
    let cache = Arc::new(Cache::new(&CacheSettings::default()).unwrap());
    let clock = Arc::new(TestClock(Mutex::new(
        UNIX_EPOCH + Duration::from_secs(1_800_000_000),
    )));
    let layer = CacheLayer::new(
        cache,
        policies,
        LayerSettings {
            name: "edge".into(),
            ..LayerSettings::default()
        },
        clock.clone(),
    );
    let proxy = Arc::new(Proxy::new(&cfg).with_filters(layer.route_filters()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(serve(
        listener,
        proxy,
        ServerLimits::default(),
        async move {
            let _ = stopped.await;
        },
    ));
    Running {
        addr,
        clock,
        _stop: stop,
    }
}

async fn get(addr: SocketAddr, path: &str) -> String {
    let mut conn = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
    conn.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

fn header<'a>(resp: &'a str, name: &str) -> Option<&'a str> {
    let at = resp.find(&format!("\r\n{name}: "))? + name.len() + 4;
    resp[at..].split("\r\n").next()
}

const CACHEABLE: &str =
    "HTTP/1.1 200 OK\r\nCache-Control: max-age=60\r\nContent-Length: 5\r\n\r\nhello";

#[tokio::test]
async fn the_cached_route_hits_and_the_other_bypasses() {
    let up = upstream(CACHEABLE, Duration::ZERO).await;
    let proxy = start(&up).await;
    let first = get(proxy.addr, "/x").await;
    assert_eq!(
        header(&first, "cache-status"),
        Some("edge; fwd=miss; fwd-status=200; stored; ttl=60")
    );
    assert!(first.ends_with("hello"), "{first}");
    proxy.clock.advance(5);
    let second = get(proxy.addr, "/x").await;
    assert_eq!(header(&second, "cache-status"), Some("edge; hit; ttl=55"));
    assert_eq!(header(&second, "age"), Some("5"));
    assert!(second.ends_with("hello"), "{second}");
    // Each answer carries its own request's ID, not the stored one's.
    let (a, b) = (
        header(&first, "x-request-id"),
        header(&second, "x-request-id"),
    );
    assert!(a.is_some() && b.is_some() && a != b, "{a:?} {b:?}");
    assert_eq!(up.requests.load(Ordering::SeqCst), 1);
    for _ in 0..2 {
        let plain = get(proxy.addr, "/plain/x").await;
        assert_eq!(header(&plain, "cache-status"), None, "{plain}");
        assert!(plain.ends_with("hello"));
    }
    assert_eq!(up.requests.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn concurrent_misses_collapse_into_one_upstream_request() {
    let up = upstream(CACHEABLE, Duration::from_millis(300)).await;
    let proxy = start(&up).await;
    let addr = proxy.addr;
    let tasks: Vec<_> = (0..10)
        .map(|_| tokio::spawn(async move { get(addr, "/c").await }))
        .collect();
    let mut collapsed = 0;
    for t in tasks {
        let resp = t.await.unwrap();
        assert!(resp.ends_with("hello"), "{resp}");
        collapsed += usize::from(header(&resp, "cache-status").unwrap().contains("collapsed"));
    }
    assert_eq!(up.requests.load(Ordering::SeqCst), 1);
    assert_eq!(collapsed, 9);
}

#[tokio::test]
async fn a_stale_copy_covers_an_upstream_that_fails() {
    let up = upstream(
        "HTTP/1.1 200 OK\r\nCache-Control: max-age=1, stale-if-error=60\r\nContent-Length: 3\r\n\r\nold",
        Duration::ZERO,
    )
    .await;
    let proxy = start(&up).await;
    get(proxy.addr, "/e").await;
    proxy.clock.advance(5);
    up.down.store(true, Ordering::SeqCst);
    let resp = get(proxy.addr, "/e").await;
    assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "{resp}");
    assert_eq!(
        header(&resp, "cache-status"),
        Some("edge; hit; ttl=-4; fwd=stale; detail-origin=bad_gateway; detail=stale-if-error")
    );
    assert!(header(&resp, "x-request-id").is_some());
    assert!(resp.ends_with("old"), "{resp}");
    // Without a copy, the proxy's own error goes through.
    let resp = get(proxy.addr, "/none").await;
    assert!(resp.starts_with("HTTP/1.1 502 Bad Gateway\r\n"), "{resp}");
    assert_eq!(
        header(&resp, "cache-status"),
        Some("edge; fwd=miss; detail=bad_gateway")
    );
}
