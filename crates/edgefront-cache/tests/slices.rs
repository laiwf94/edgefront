//! Large objects cached as slices, over real TCP: a 5 MiB object on a route
//! that stores at most 1 MiB, in 256 KiB slices, in front of an origin that
//! honours Range and can change the object under the same URL.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use edgefront_cache::config::CacheConfig;
use edgefront_cache::layer::{CacheLayer, Clock, LayerSettings};
use edgefront_cache::purge::PurgeTarget;
use edgefront_cache::slice::slice_key;
use edgefront_cache::store::{Cache, CacheSettings};
use edgefront_core::config::Config;
use edgefront_core::listener::{ServerLimits, serve};
use edgefront_proxy::server::Proxy;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

const LEN: u64 = 5 << 20;
const SLICE: u64 = 256 << 10;

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }
}

fn byte(version: u64, i: u64) -> u8 {
    ((i + version * 7) % 251) as u8
}

fn content(version: u64, start: u64, end: u64) -> Vec<u8> {
    (start..=end).map(|i| byte(version, i)).collect()
}

/// An origin serving one `LEN`-byte object at version `version`, whole or
/// one range, slowly when `pace` is set; it records each request's Range.
struct Origin {
    addr: String,
    version: Arc<AtomicU64>,
    pace: Arc<AtomicU64>,
    log: Arc<Mutex<Vec<Option<String>>>>,
}

async fn origin() -> Origin {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let version = Arc::new(AtomicU64::new(1));
    let pace = Arc::new(AtomicU64::new(0));
    let log = Arc::new(Mutex::new(Vec::new()));
    let (v, p, l) = (version.clone(), pace.clone(), log.clone());
    tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            let (v, p, l) = (v.clone(), p.clone(), l.clone());
            tokio::spawn(async move {
                let (r, mut w) = conn.into_split();
                let mut r = BufReader::new(r);
                loop {
                    let mut range = None;
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                        if let Some(value) = line.to_ascii_lowercase().strip_prefix("range:") {
                            range = Some(value.trim().to_owned());
                        }
                    }
                    l.lock().unwrap().push(range.clone());
                    let version = v.load(Ordering::SeqCst);
                    let (status, start, end) =
                        match range.as_deref().and_then(|r| r.strip_prefix("bytes=")) {
                            Some(spec) => {
                                let (a, b) = spec.split_once('-').unwrap();
                                let a: u64 = a.parse().unwrap();
                                let b: u64 = b.parse::<u64>().unwrap_or(LEN - 1).min(LEN - 1);
                                ("206 Partial Content", a, b)
                            }
                            None => ("200 OK", 0, LEN - 1),
                        };
                    let mut head = format!(
                        "HTTP/1.1 {status}\r\ncache-control: max-age=600\r\netag: \"v{version}\"\r\nsurrogate-key: video\r\ncontent-length: {}\r\n",
                        end - start + 1
                    );
                    if status.starts_with("206") {
                        head.push_str(&format!("content-range: bytes {start}-{end}/{LEN}\r\n"));
                    }
                    head.push_str("\r\n");
                    if w.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    let body = content(version, start, end);
                    for chunk in body.chunks(64 << 10) {
                        if w.write_all(chunk).await.is_err() {
                            return;
                        }
                        let ms = p.load(Ordering::SeqCst);
                        if ms > 0 {
                            let _ = w.flush().await;
                            tokio::time::sleep(Duration::from_millis(ms)).await;
                        }
                    }
                }
            });
        }
    });
    Origin {
        addr,
        version,
        pace,
        log,
    }
}

async fn start(up: &Origin) -> (SocketAddr, Arc<Cache>, oneshot::Sender<()>) {
    let config = format!(
        "{{\"listen\":\"127.0.0.1:0\",\"routes\":[\
         {{\"host\":\"a\",\"path_prefix\":\"/\",\"cluster\":\"web\"}}],\
         \"clusters\":[{{\"name\":\"web\",\"endpoints\":[{{\"address\":\"{}\"}}]}}]}}",
        up.addr
    );
    let cfg = Config::from_json(config.as_bytes()).expect("a valid config");
    let cache_cfg: CacheConfig = toml::from_str(&format!(
        "max_object_bytes = {}\nslice_bytes = {SLICE}\nroutes = [{{ host = \"a\", path_prefix = \"/\" }}]",
        1 << 20
    ))
    .unwrap();
    let policies = cache_cfg.resolve(&cfg.routes).unwrap();
    let cache = Arc::new(
        Cache::new(&CacheSettings {
            memory_bytes: 64 << 20,
            shards: 1,
            max_object_bytes: 1 << 20,
            ..CacheSettings::default()
        })
        .unwrap(),
    );
    let layer = CacheLayer::new(
        cache.clone(),
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
    (addr, cache, stop)
}

struct Seen {
    head: String,
    first_byte: Option<Instant>,
    body: Vec<u8>,
}

async fn fetch(addr: SocketAddr, extra: &str) -> Seen {
    let conn = TcpStream::connect(addr).await.unwrap();
    let (r, mut w) = conn.into_split();
    let req = format!("GET /video HTTP/1.1\r\nHost: a\r\nConnection: close\r\n{extra}\r\n");
    w.write_all(req.as_bytes()).await.unwrap();
    let mut r = BufReader::new(r);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        head.push_str(&line.to_ascii_lowercase());
    }
    let mut first = [0u8; 1];
    let (first_byte, mut body) = match r.read_exact(&mut first).await {
        Ok(_) => (Some(Instant::now()), first.to_vec()),
        Err(_) => (None, Vec::new()),
    };
    let _ = tokio::time::timeout(Duration::from_secs(30), r.read_to_end(&mut body)).await;
    Seen {
        head,
        first_byte,
        body,
    }
}

fn requests(up: &Origin) -> Vec<Option<String>> {
    up.log.lock().unwrap().clone()
}

async fn wait_stored(cache: &Cache) {
    for _ in 0..500 {
        if cache.contains("a/video") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the manifest was never stored");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_object_is_stored_as_slices_and_served_from_them() {
    let up = origin().await;
    let (addr, cache, _stop) = start(&up).await;
    let first = fetch(addr, "").await;
    assert!(first.head.starts_with("http/1.1 200"), "{}", first.head);
    assert!(first.head.contains(&format!("content-length: {LEN}")));
    assert!(first.body == content(1, 0, LEN - 1));
    wait_stored(&cache).await;
    // Every slice is stored under its own key.
    for i in 0..LEN / SLICE {
        assert!(cache.contains(&slice_key("a/video", i)), "slice {i}");
    }
    // Whole and ranged requests come from the slices.
    let whole = fetch(addr, "").await;
    assert!(whole.head.contains("edge; hit"), "{}", whole.head);
    assert!(!whole.head.contains("edgefront-slices") && !whole.head.contains("surrogate-key"));
    assert!(whole.body == content(1, 0, LEN - 1));
    let across = fetch(addr, "Range: bytes=262000-786500\r\n").await;
    assert!(across.head.starts_with("http/1.1 206"), "{}", across.head);
    assert!(
        across
            .head
            .contains(&format!("content-range: bytes 262000-786500/{LEN}"))
    );
    assert!(across.body == content(1, 262_000, 786_500));
    let tail = fetch(addr, "Range: bytes=-10\r\n").await;
    assert!(tail.body == content(1, LEN - 10, LEN - 1));
    let none = fetch(addr, &format!("Range: bytes={LEN}-\r\n")).await;
    assert!(none.head.starts_with("http/1.1 416"), "{}", none.head);
    let inm = fetch(addr, "If-None-Match: \"v1\"\r\n").await;
    assert!(inm.head.starts_with("http/1.1 304"), "{}", inm.head);
    assert_eq!(requests(&up), [None]);
    // A purge of the URL takes the slices with it.
    cache.purge(&[PurgeTarget::Key("a/video".into())]).await;
    assert!(!cache.contains("a/video") && !cache.contains(&slice_key("a/video", 3)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_slice_is_fetched_with_an_aligned_range() {
    let up = origin().await;
    let (addr, cache, _stop) = start(&up).await;
    fetch(addr, "").await;
    wait_stored(&cache).await;
    cache.memory().remove(&slice_key("a/video", 5));
    let got = fetch(addr, "Range: bytes=1300000-1300099\r\n").await;
    assert!(got.head.starts_with("http/1.1 206"), "{}", got.head);
    assert!(got.body == content(1, 1_300_000, 1_300_099));
    // Bytes 1300000-1300099 lie in slice 4, which is still held: no fetch.
    assert_eq!(requests(&up).len(), 1);
    let got = fetch(addr, "Range: bytes=1400000-1400099\r\n").await;
    assert!(got.body == content(1, 1_400_000, 1_400_099));
    assert_eq!(
        requests(&up),
        [None, Some(format!("bytes={}-{}", 5 * SLICE, 6 * SLICE - 1))]
    );
    // The slice is stored once all of it is in, after the client's bytes.
    for _ in 0..500 {
        if cache.contains(&slice_key("a/video", 5)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(cache.contains(&slice_key("a/video", 5)));
    // Now held again: no third request.
    fetch(addr, "Range: bytes=1400000-1400099\r\n").await;
    assert_eq!(requests(&up).len(), 2);
    // A whole request with a slice gone fetches the object again.
    cache.memory().remove(&slice_key("a/video", 9));
    let whole = fetch(addr, "").await;
    assert!(whole.body == content(1, 0, LEN - 1));
    assert_eq!(requests(&up)[2], None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slices_of_a_changed_object_are_never_mixed() {
    let up = origin().await;
    let (addr, cache, _stop) = start(&up).await;
    fetch(addr, "").await;
    wait_stored(&cache).await;
    // The origin's object changes; one old slice goes missing.
    up.version.store(2, Ordering::SeqCst);
    cache.memory().remove(&slice_key("a/video", 2));
    let got = fetch(addr, "Range: bytes=600000-600009\r\n").await;
    // The new bytes, from the fetched slice.
    assert!(got.body == content(2, 600_000, 600_009));
    // The old slices went with the old manifest.
    assert!(!cache.contains(&slice_key("a/video", 0)));
    // A range over a slice the new object has not had fetched yet goes to
    // the origin and gets new bytes, never an old slice.
    let other = fetch(addr, "Range: bytes=10-19\r\n").await;
    assert!(other.body == content(2, 10, 19));
    assert_eq!(requests(&up).len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_joining_a_slice_fill_gets_its_first_byte_at_once() {
    let up = origin().await;
    // 64 KiB every 20 ms: the 5 MiB body takes about 1.6 s to arrive.
    up.pace.store(20, Ordering::SeqCst);
    let (addr, cache, _stop) = start(&up).await;
    let leader = tokio::spawn(async move { fetch(addr, "").await });
    tokio::time::sleep(Duration::from_millis(400)).await;
    let joined = Instant::now();
    let follower = fetch(addr, "").await;
    let leader = leader.await.unwrap();
    let waited = follower.first_byte.unwrap().duration_since(joined);
    assert!(
        waited < Duration::from_millis(50),
        "first byte after {waited:?}"
    );
    assert!(follower.head.contains("collapsed"), "{}", follower.head);
    assert!(leader.body == content(1, 0, LEN - 1));
    assert!(follower.body == content(1, 0, LEN - 1));
    assert_eq!(requests(&up).len(), 1);
    wait_stored(&cache).await;
}
