//! End to end over HTTP/2 against the binary: all four gRPC call types
//! through `edgefront proxy` to a tonic echo service, a browser-style page
//! load over TLS with one connection carrying the page and its assets,
//! and what proxying HTTP/2 adds to a call on loopback.

#[path = "../../edgefront-proxy/tests/common/grpc_echo.rs"]
mod echo;
#[path = "../../edgefront-core/tests/common/tls.rs"]
mod tlsc;

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use edgefront_core::config::Http2;
use edgefront_core::h2::Settings;
use edgefront_core::listener::{Handler, ServerLimits, serve};
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};

const EDGEFRONT: &str = env!("CARGO_BIN_EXE_edgefront");
const TIMEOUT: Duration = Duration::from_secs(10);

/// A running `edgefront proxy`, stopped with SIGTERM when dropped.
struct Proc {
    child: Child,
    proxy: SocketAddr,
}

impl Drop for Proc {
    fn drop(&mut self) {
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
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(config: &Path) -> Proc {
    let mut child = Command::new(EDGEFRONT)
        .args(["proxy", "--config"])
        .arg(config)
        .env_remove("EDGEFRONT_ADMIN_TOKEN")
        .env("EDGEFRONT_DRAIN_PERIOD", "0s")
        .env("EDGEFRONT_LOG", "warn,edgefront=info,edgefront_core=info")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start the proxy");
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = mpsc::channel::<String>();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + TIMEOUT;
    let line = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match lines.recv_timeout(left) {
            Ok(line) if line.contains("\"listening\"") => break line,
            Ok(_) => {}
            Err(_) => panic!("edgefront did not start"),
        }
    };
    let key = "\"proxy\":\"";
    let at = line.find(key).unwrap() + key.len();
    let proxy = line[at..].split('"').next().unwrap().parse().unwrap();
    // Keep draining the log so the process never blocks on stdout.
    thread::spawn(move || while lines.recv().is_ok() {});
    Proc { child, proxy }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("e2e-h2-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Answers `GET /<name>` with a body naming it and the version it came
/// over, after `HTTP/2.0 ` or `HTTP/1.1 `.
struct Assets;

impl Handler for Assets {
    type Body = Full<Bytes>;

    async fn call(
        self: Arc<Self>,
        req: http::Request<Incoming>,
        _: SocketAddr,
    ) -> http::Response<Full<Bytes>> {
        let text = format!("{:?} {}", req.version(), req.uri().path());
        http::Response::builder()
            .header("content-type", "text/plain")
            .body(Full::new(Bytes::from(text)))
            .unwrap()
    }
}

/// An origin for `Assets` that takes HTTP/1.1 and prior-knowledge HTTP/2.
async fn assets() -> SocketAddr {
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let limits = ServerLimits {
        http2: Some(Settings::from(&Http2 {
            cleartext: true,
            ..Http2::default()
        })),
        ..ServerLimits::default()
    };
    tokio::spawn(serve(
        socket,
        Arc::new(Assets),
        limits,
        std::future::pending(),
    ));
    addr
}

/// A cleartext proxy taking prior-knowledge HTTP/2: `/echo.Echo/` to the
/// gRPC service, everything else to the assets, both over HTTP/2.
fn cleartext_proxy(dir: &Path, grpc: SocketAddr, web: SocketAddr) -> Proc {
    let config = dir.join("proxy.json");
    let js = serde_json::json!({
        "listen": "127.0.0.1:0",
        "admin_listen": "127.0.0.1:0",
        "http2": {"cleartext": true},
        "routes": [
            {"path_prefix": "/echo.Echo/", "cluster": "grpc"},
            {"cluster": "web"},
        ],
        "clusters": [
            {"name": "grpc", "protocol": "http2", "timeout": "0s", "endpoints": [{"address": grpc.to_string()}]},
            {"name": "web", "protocol": "http2", "endpoints": [{"address": web.to_string()}]},
        ],
    });
    std::fs::write(&config, js.to_string()).unwrap();
    start(&config)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_four_grpc_call_types_through_the_binary() {
    let dir = scratch("grpc");
    let proc = cleartext_proxy(&dir, echo::start().await, assets().await);
    let mut grpc = echo::client(proc.proxy).await;

    let unary = echo::unary(&mut grpc, "hello", None).await.unwrap();
    assert_eq!(&unary.get_ref()[..], b"hello");
    let streamed = echo::server_stream(&mut grpc, 50).await.unwrap();
    assert_eq!(streamed.len(), 50);
    assert_eq!(streamed[49], "49");
    let joined = echo::client_stream(&mut grpc, &["x", "y", "z"])
        .await
        .unwrap();
    assert_eq!(&joined[..], b"x,y,z");
    let bidi = echo::bidi(&mut grpc, &["a", "bb", "ccc"]).await.unwrap();
    assert_eq!(bidi, ["A", "BB", "CCC"]);

    // A status with its trailer metadata, as the service sent it.
    let err = echo::unary(&mut grpc, "fail", None).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
    assert_eq!(err.message(), "nothing here");
    assert_eq!(err.metadata().get("x-detail").unwrap(), "why");
    // The time left reaches the service.
    let timed = echo::unary(&mut grpc, "t", Some(Duration::from_secs(2)))
        .await
        .unwrap();
    let seen = timed
        .metadata()
        .get("seen-timeout")
        .unwrap()
        .to_str()
        .unwrap();
    let ms: u64 = seen.strip_suffix('m').unwrap().parse().unwrap();
    assert!((1_900..=2_000).contains(&ms), "{seen}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_page_and_its_assets_load_over_one_tls_connection() {
    let dir = scratch("page");
    let web = assets().await;
    let config = dir.join("proxy.json");
    let js = serde_json::json!({
        "listen": "127.0.0.1:0",
        "admin_listen": "127.0.0.1:0",
        "http2": {},
        "tls": {"certificates": [{"cert_file": tlsc::fixture("exact.crt"), "key_file": tlsc::fixture("exact.key")}]},
        "routes": [{"cluster": "web"}],
        "clusters": [{"name": "web", "protocol": "auto", "endpoints": [{"address": web.to_string()}]}],
    });
    std::fs::write(&config, js.to_string()).unwrap();
    let proc = start(&config);

    // A browser offers h2 and http/1.1 and gets h2.
    let tls = tlsc::handshake(
        proc.proxy,
        "api.example.com",
        &tlsc::ClientOpts {
            alpn: vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            ..tlsc::ClientOpts::default()
        },
    )
    .await
    .expect("a handshake");
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    let (sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
            .await
            .unwrap();
    tokio::spawn(conn);
    let get = |path: String| {
        let mut sender = sender.clone();
        async move {
            let req = http::Request::get(format!("https://api.example.com{path}"))
                .body(Empty::<Bytes>::new())
                .unwrap();
            let resp = sender.send_request(req).await.unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.version(), http::Version::HTTP_2);
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            String::from_utf8_lossy(&body).into_owned()
        }
    };
    // The page, then its 60 stylesheets, scripts and images at once.
    let start = Instant::now();
    assert_eq!(get("/index.html".to_owned()).await, "HTTP/2.0 /index.html");
    let mut assets = Vec::new();
    for i in 0..60 {
        let kind = ["css", "js", "png"][i % 3];
        assets.push(tokio::spawn(get(format!("/static/{i}.{kind}"))));
    }
    for (i, a) in assets.into_iter().enumerate() {
        let kind = ["css", "js", "png"][i % 3];
        // The proxy reached the origin over HTTP/2 too (protocol auto).
        assert_eq!(a.await.unwrap(), format!("HTTP/2.0 /static/{i}.{kind}"));
    }
    eprintln!(
        "page and 60 assets over one connection in {:?}",
        start.elapsed()
    );
}

/// The p50 of `n` sequential GETs on one HTTP/2 connection to `addr`.
async fn p50(addr: SocketAddr, n: usize) -> Duration {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let _ = tcp.set_nodelay(true);
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
            .await
            .unwrap();
    tokio::spawn(conn);
    let mut times = Vec::with_capacity(n);
    for i in 0..n + 200 {
        let req = http::Request::get(format!("http://edge.test/{}", i % 100))
            .body(Empty::<Bytes>::new())
            .unwrap();
        let begun = Instant::now();
        let resp = sender.send_request(req).await.unwrap();
        let _ = resp.into_body().collect().await.unwrap();
        // The first 200 warm the connections and the pool up.
        if i >= 200 {
            times.push(begun.elapsed());
        }
    }
    times.sort();
    times[n / 2]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proxying_http2_adds_little_to_a_call() {
    let dir = scratch("overhead");
    let web = assets().await;
    let proc = cleartext_proxy(&dir, echo::start().await, web);
    let direct = p50(web, 5_000).await;
    let proxied = p50(proc.proxy, 5_000).await;
    let added = proxied.saturating_sub(direct);
    eprintln!(
        "HTTP/2 p50 on loopback: direct {direct:?}, through edgefront {proxied:?}, added {added:?}"
    );
    // The budget holds for an optimized build; a debug build only checks
    // that the calls work.
    if !cfg!(debug_assertions) {
        assert!(added < Duration::from_micros(60), "added {added:?}");
    }
}
