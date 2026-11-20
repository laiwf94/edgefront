//! Streaming fill over real TCP: a client that arrives while a large
//! object is still coming from the origin gets the bytes already in at
//! once and the rest as they arrive, and an origin that fails partway
//! fails every client's response and stores nothing.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use edgefront_cache::config::CacheConfig;
use edgefront_cache::layer::{CacheLayer, Clock, LayerSettings};
use edgefront_cache::store::{Cache, CacheSettings};
use edgefront_core::config::Config;
use edgefront_core::listener::{ServerLimits, serve};
use edgefront_proxy::server::Proxy;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, oneshot};

const LEN: usize = 200_000;
const FIRST: usize = 1_000;

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }
}

/// An origin that sends the head and the first `FIRST` bytes of a
/// `LEN`-byte body, records when, and holds the rest until `release`; or,
/// when `fail`, closes the connection instead.
struct Origin {
    addr: String,
    requests: Arc<AtomicUsize>,
    first_byte: Arc<Mutex<Option<Instant>>>,
    release: Arc<Notify>,
}

fn byte(i: usize) -> u8 {
    (i % 251) as u8
}

async fn origin(fail: bool) -> Origin {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let requests = Arc::new(AtomicUsize::new(0));
    let first_byte = Arc::new(Mutex::new(None));
    let release = Arc::new(Notify::new());
    let (n, fb, rel) = (requests.clone(), first_byte.clone(), release.clone());
    tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            let (n, fb, rel) = (n.clone(), fb.clone(), rel.clone());
            tokio::spawn(async move {
                let (r, mut w) = conn.into_split();
                let mut r = BufReader::new(r);
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
                let body: Vec<u8> = (0..LEN).map(byte).collect();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncache-control: max-age=60\r\ncontent-length: {LEN}\r\n\r\n"
                );
                w.write_all(head.as_bytes()).await.unwrap();
                w.write_all(&body[..FIRST]).await.unwrap();
                w.flush().await.unwrap();
                *fb.lock().unwrap() = Some(Instant::now());
                rel.notified().await;
                if fail {
                    // Half the rest, then the connection goes away.
                    let _ = w.write_all(&body[FIRST..LEN / 2]).await;
                    return;
                }
                let _ = w.write_all(&body[FIRST..]).await;
            });
        }
    });
    Origin {
        addr,
        requests,
        first_byte,
        release,
    }
}

async fn start(up: &Origin) -> (SocketAddr, oneshot::Sender<()>) {
    let config = format!(
        "{{\"listen\":\"127.0.0.1:0\",\"routes\":[\
         {{\"host\":\"a\",\"path_prefix\":\"/\",\"cluster\":\"web\"}}],\
         \"clusters\":[{{\"name\":\"web\",\"endpoints\":[{{\"address\":\"{}\"}}]}}]}}",
        up.addr
    );
    let cfg = Config::from_json(config.as_bytes()).expect("a valid config");
    let cache_cfg: CacheConfig =
        toml::from_str("routes = [{ host = \"a\", path_prefix = \"/\" }]").unwrap();
    let policies = cache_cfg.resolve(&cfg.routes).unwrap();
    let cache = Arc::new(Cache::new(&CacheSettings::default()).unwrap());
    let layer = CacheLayer::new(
        cache,
        policies,
        LayerSettings {
            name: "edge".into(),
            ..LayerSettings::default()
        },
        Arc::new(FixedClock),
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
    (addr, stop)
}

/// What one client saw: the head, when its first body byte came, and the
/// body bytes before the connection ended.
struct Seen {
    head: String,
    first_byte: Instant,
    body: Vec<u8>,
}

async fn fetch(addr: SocketAddr, extra: &str) -> Seen {
    let conn = TcpStream::connect(addr).await.unwrap();
    let (r, mut w) = conn.into_split();
    let req = format!("GET /big HTTP/1.1\r\nHost: a\r\nConnection: close\r\n{extra}\r\n");
    w.write_all(req.as_bytes()).await.unwrap();
    let mut r = BufReader::new(r);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        head.push_str(&line);
    }
    let mut first = [0u8; 1];
    r.read_exact(&mut first).await.unwrap();
    let first_byte = Instant::now();
    let mut body = first.to_vec();
    let _ = tokio::time::timeout(Duration::from_secs(10), r.read_to_end(&mut body)).await;
    Seen {
        head,
        first_byte,
        body,
    }
}

async fn origin_started(up: &Origin) -> Instant {
    for _ in 0..500 {
        if let Some(t) = *up.first_byte.lock().unwrap() {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("the origin never sent its first byte");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_joining_mid_fill_streams_from_the_first_byte() {
    let up = origin(false).await;
    let (addr, _stop) = start(&up).await;
    let leader = tokio::spawn(async move { fetch(addr, "").await });
    let origin_first = origin_started(&up).await;
    let follower = tokio::spawn(async move { fetch(addr, "").await });
    // The rest of the body is held back for half a second, so a follower
    // that waited for the whole object would get its first byte late.
    tokio::time::sleep(Duration::from_millis(500)).await;
    up.release.notify_one();
    let (leader, follower) = (leader.await.unwrap(), follower.await.unwrap());
    let waited = follower.first_byte.duration_since(origin_first);
    assert!(
        waited < Duration::from_millis(50),
        "first byte {waited:?} after the origin's"
    );
    let want: Vec<u8> = (0..LEN).map(byte).collect();
    for seen in [&leader, &follower] {
        assert!(seen.head.starts_with("HTTP/1.1 200"), "{}", seen.head);
        assert!(
            seen.head
                .to_ascii_lowercase()
                .contains(&format!("content-length: {LEN}")),
            "{}",
            seen.head
        );
        assert_eq!(seen.body.len(), LEN);
        assert!(seen.body == want);
    }
    assert!(follower.head.contains("collapsed"), "{}", follower.head);
    assert_eq!(up.requests.load(Ordering::SeqCst), 1);
    // Stored once complete: the next request is a hit.
    let mut hit = None;
    for _ in 0..100 {
        let seen = fetch(addr, "").await;
        if seen.head.contains("edge; hit") {
            hit = Some(seen);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let hit = hit.expect("the streamed object was not stored");
    assert!(hit.body == want);
    assert_eq!(up.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_follower_with_a_range_waits_for_the_whole_object() {
    let up = origin(false).await;
    let (addr, _stop) = start(&up).await;
    let leader = tokio::spawn(async move { fetch(addr, "").await });
    origin_started(&up).await;
    let ranged = tokio::spawn(async move { fetch(addr, "Range: bytes=150000-150009\r\n").await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    up.release.notify_one();
    let (leader, ranged) = (leader.await.unwrap(), ranged.await.unwrap());
    assert_eq!(leader.body.len(), LEN);
    assert!(ranged.head.starts_with("HTTP/1.1 206"), "{}", ranged.head);
    let want: Vec<u8> = (150_000..150_010).map(byte).collect();
    assert_eq!(ranged.body, want);
    assert_eq!(up.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_origin_failing_mid_body_fails_every_client_and_stores_nothing() {
    let up = origin(true).await;
    let (addr, _stop) = start(&up).await;
    let leader = tokio::spawn(async move { fetch(addr, "").await });
    origin_started(&up).await;
    let follower = tokio::spawn(async move { fetch(addr, "").await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    up.release.notify_one();
    let (leader, follower) = (leader.await.unwrap(), follower.await.unwrap());
    // Both connections end short of the declared length: a broken
    // response, never one taken for whole.
    for seen in [&leader, &follower] {
        assert!(seen.body.len() < LEN, "{} bytes", seen.body.len());
    }
    // Nothing was stored, so the next request goes to the origin again.
    let before = up.requests.load(Ordering::SeqCst);
    *up.first_byte.lock().unwrap() = None;
    let again = tokio::spawn(async move { fetch(addr, "").await });
    origin_started(&up).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    up.release.notify_one();
    let _ = again.await.unwrap();
    assert_eq!(up.requests.load(Ordering::SeqCst), before + 1);
}
