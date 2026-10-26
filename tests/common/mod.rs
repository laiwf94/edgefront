//! Test origins: small HTTP servers whose behaviour each test scripts.
#![allow(dead_code)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::request::Parts;
use http::{HeaderMap, Method, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::task::{JoinHandle, JoinSet};

/// Wall clock with an adjustable offset, so tests can age cached objects
/// without sleeping while origin Date headers stay realistic.
#[derive(Default)]
pub struct TestClock {
    offset: Mutex<Duration>,
}

impl TestClock {
    pub fn advance(&self, d: Duration) {
        *self.offset.lock().unwrap() += d;
    }
}

impl cachet::proxy::Clock for TestClock {
    fn now(&self) -> std::time::SystemTime {
        std::time::SystemTime::now() + *self.offset.lock().unwrap()
    }
}

pub struct Harness {
    pub proxy: Arc<cachet::proxy::Proxy>,
    pub clock: Arc<TestClock>,
}

/// A proxy in front of `origins`, with settings adjusted by `tweak`.
pub fn harness(
    origins: &[SocketAddr],
    tweak: impl FnOnce(&mut cachet::store::CacheSettings, &mut cachet::proxy::ProxySettings),
) -> Harness {
    use cachet::origin::{HealthSettings, Origin, OriginPool, client};
    let mut cache = cachet::store::CacheSettings {
        memory_bytes: 64 << 20,
        shards: 4,
        ..Default::default()
    };
    let mut settings = cachet::proxy::ProxySettings {
        node_id: "test-node".into(),
        ..Default::default()
    };
    tweak(&mut cache, &mut settings);
    let origins = origins
        .iter()
        .enumerate()
        .map(|(i, a)| {
            Arc::new(Origin::new(
                &format!("origin{i}"),
                a.to_string().parse().unwrap(),
            ))
        })
        .collect();
    let pool = Arc::new(OriginPool::new(
        origins,
        client(Duration::from_millis(500)),
        Duration::from_secs(2),
        HealthSettings::default(),
    ));
    let clock = Arc::new(TestClock::default());
    let proxy = cachet::proxy::Proxy::new(
        cachet::store::Cache::new(&cache).unwrap(),
        pool,
        settings,
        clock.clone(),
    );
    Harness { proxy, clock }
}

/// A response as the client saw it.
#[derive(Debug)]
pub struct Got {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Got {
    pub fn cache_status(&self) -> &str {
        self.headers
            .get("cache-status")
            .map_or("", |v| v.to_str().unwrap())
    }

    pub fn header(&self, name: &str) -> &str {
        self.headers.get(name).map_or("", |v| v.to_str().unwrap())
    }
}

impl Harness {
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &'static str,
    ) -> Got {
        let mut b = http::Request::builder().method(method).uri(path);
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            b = b.header("host", "example.com");
        }
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let req = b
            .body(Full::new(Bytes::from_static(body.as_bytes())))
            .unwrap();
        let resp = self
            .proxy
            .handle(req, "192.0.2.1:5000".parse().unwrap())
            .await;
        let (parts, body) = resp.into_parts();
        let body = body
            .collect()
            .await
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        Got {
            status: parts.status,
            headers: parts.headers,
            body,
        }
    }

    pub async fn get(&self, path: &str, headers: &[(&str, &str)]) -> Got {
        self.send("GET", path, headers, "").await
    }
}

pub type Handler = Arc<dyn Fn(&Parts, &Bytes) -> Response<Full<Bytes>> + Send + Sync>;

/// What the origin saw.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Bytes,
}

pub struct TestOrigin {
    pub addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Seen>>>,
    delay: Arc<Mutex<Duration>>,
    handler: Arc<Mutex<Handler>>,
    clock: Arc<Mutex<Option<Arc<TestClock>>>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

pub fn handler(
    f: impl Fn(&Parts, &Bytes) -> Response<Full<Bytes>> + Send + Sync + 'static,
) -> Handler {
    Arc::new(f)
}

/// A response with a body and header pairs.
pub fn reply(
    status: u16,
    headers: &[(&str, &str)],
    body: impl Into<Bytes>,
) -> Response<Full<Bytes>> {
    let mut b = Response::builder().status(StatusCode::from_u16(status).unwrap());
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Full::new(body.into())).unwrap()
}

impl TestOrigin {
    pub async fn start(h: Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let origin = TestOrigin {
            addr,
            hits: Arc::new(AtomicUsize::new(0)),
            seen: Arc::new(Mutex::new(Vec::new())),
            delay: Arc::new(Mutex::new(Duration::ZERO)),
            handler: Arc::new(Mutex::new(h)),
            clock: Arc::new(Mutex::new(None)),
            task: Mutex::new(None),
        };
        origin.serve(listener);
        origin
    }

    fn serve(&self, listener: TcpListener) {
        let state = Served {
            hits: self.hits.clone(),
            seen: self.seen.clone(),
            delay: self.delay.clone(),
            handler: self.handler.clone(),
            clock: self.clock.clone(),
        };
        let task = tokio::spawn(async move {
            let mut conns = JoinSet::new();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let state = state.clone();
                conns.spawn(async move {
                    let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(state.answer(req).await) }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        *self.task.lock().unwrap() = Some(task);
    }

    /// Stops accepting and drops every open connection.
    pub fn stop(&self) {
        if let Some(t) = self.task.lock().unwrap().take() {
            t.abort();
        }
    }

    /// Starts serving again on the same address.
    pub async fn restart(&self) {
        self.stop();
        let mut tries = 0;
        let listener = loop {
            match TcpListener::bind(self.addr).await {
                Ok(l) => break l,
                Err(e) if tries < 50 => {
                    tries += 1;
                    let _ = e;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("rebind {}: {e}", self.addr),
            }
        };
        self.serve(listener);
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub fn last(&self) -> Seen {
        self.seen().pop().expect("origin saw no request")
    }

    pub fn set_delay(&self, d: Duration) {
        *self.delay.lock().unwrap() = d;
    }

    /// Dates responses from `clock`, so the origin and proxy agree on time.
    pub fn use_clock(&self, clock: &Arc<TestClock>) {
        *self.clock.lock().unwrap() = Some(clock.clone());
    }

    pub fn set_handler(&self, h: Handler) {
        *self.handler.lock().unwrap() = h;
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

#[derive(Clone)]
struct Served {
    hits: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Seen>>>,
    delay: Arc<Mutex<Duration>>,
    handler: Arc<Mutex<Handler>>,
    clock: Arc<Mutex<Option<Arc<TestClock>>>>,
}

impl Served {
    async fn answer(&self, req: hyper::Request<hyper::body::Incoming>) -> Response<Chunked> {
        let (parts, body) = req.into_parts();
        let body = body
            .collect()
            .await
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        // Health probes are not counted as hits and are never delayed.
        let probe = parts.uri.path() == "/healthz";
        if !probe {
            self.hits.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(Seen {
                method: parts.method.clone(),
                uri: parts.uri.clone(),
                headers: parts.headers.clone(),
                body: body.clone(),
            });
            let d = *self.delay.lock().unwrap();
            if !d.is_zero() {
                tokio::time::sleep(d).await;
            }
        }
        let h = self.handler.lock().unwrap().clone();
        let mut resp = h(&parts, &body);
        let clock = self.clock.lock().unwrap().clone();
        if let Some(clock) = clock
            && !resp.headers().contains_key("date")
        {
            use cachet::proxy::Clock;
            resp.headers_mut()
                .insert("date", cachet::headers::http_date(clock.now()));
        }
        let chunked = resp.headers().contains_key("x-test-chunked");
        resp.map(|b| Chunked { inner: b, chunked })
    }
}

/// A body that hides its length when asked, so hyper sends it chunked.
struct Chunked {
    inner: Full<Bytes>,
    chunked: bool,
}

impl hyper::body::Body for Chunked {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Infallible>>> {
        std::pin::Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        if self.chunked {
            hyper::body::SizeHint::default()
        } else {
            self.inner.size_hint()
        }
    }

    fn is_end_stream(&self) -> bool {
        !self.chunked && self.inner.is_end_stream()
    }
}

impl Drop for TestOrigin {
    fn drop(&mut self) {
        self.stop();
    }
}

/// An address nothing listens on.
pub async fn closed_port() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap()
}

/// A server that accepts connections and never answers.
pub async fn black_hole() -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });
    (addr, task)
}

/// A server that answers every connection with bytes that are not HTTP.
pub async fn garbage_server() -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let _ = s.write_all(b"this is not http\r\n\r\n").await;
            let _ = s.shutdown().await;
        }
    });
    (addr, task)
}

/// A server that sends cacheable response headers, then stalls mid-body.
pub async fn stalled_body() -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut s, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await;
            let head = "HTTP/1.1 200 OK\r\ncache-control: max-age=60\r\ncontent-length: 100\r\n\r\npartial";
            let _ = s.write_all(head.as_bytes()).await;
            held.push(s);
        }
    });
    (addr, task)
}

/// Polls an async check until it is true or a few seconds pass.
pub async fn eventually_async<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..300 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Polls `f` until it is true or a few seconds pass.
pub async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..300 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Sends a request over TCP, as a real client would.
pub async fn http(
    method: &str,
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    body: &'static str,
) -> Got {
    http_on(
        &cachet::origin::client(Duration::from_secs(2)),
        method,
        addr,
        path,
        headers,
        body,
    )
    .await
}

/// Like `http`, on a client the caller keeps, so connections are reused.
pub async fn http_on(
    client: &cachet::origin::Client,
    method: &str,
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    body: &'static str,
) -> Got {
    let mut b = http::Request::builder()
        .method(method)
        .uri(format!("http://{addr}{path}"));
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        b = b.header("host", "example.com");
    }
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = b.body(cachet::respond::full(body)).unwrap();
    let resp = client.request(req).await.expect("request failed");
    let (parts, body) = resp.into_parts();
    let body = body
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    Got {
        status: parts.status,
        headers: parts.headers,
        body,
    }
}

/// A node config on ephemeral localhost ports in front of `origins`.
pub fn node_config(node_id: &str, origins: &[SocketAddr], extra: &str) -> cachet::config::Config {
    let mut text = format!(
        "node_id = \"{node_id}\"\nlisten = \"127.0.0.1:0\"\nadmin_listen = \"127.0.0.1:0\"\n\
         [health]\ninterval_ms = 50\ntimeout_ms = 200\n\
         [timeouts]\nconnect_ms = 200\nresponse_ms = 2000\nshutdown_ms = 2000\n"
    );
    for (i, o) in origins.iter().enumerate() {
        text.push_str(&format!(
            "[[origins]]\nname = \"origin{i}\"\nurl = \"http://{o}\"\n"
        ));
    }
    text.push_str(extra);
    cachet::config::Config::parse(&text).unwrap()
}

pub const TOKEN: &str = "test-admin-token";

pub fn secrets() -> cachet::config::Secrets {
    cachet::config::Secrets {
        admin_token: Some(TOKEN.into()),
        cluster_secret: Some(b"0123456789abcdef0123456789abcdef".to_vec()),
    }
}

/// The real cachet binary, started with a config file.
pub struct Binary {
    pub child: std::process::Child,
    pub proxy_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    pub lines: std::sync::mpsc::Receiver<String>,
}

impl Binary {
    /// Writes `config` to `dir`, starts the binary, and waits for its
    /// "cachet started" log line to learn the bound addresses.
    pub fn start(dir: &std::path::Path, config: &str, env: &[(&str, &str)]) -> Binary {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("cachet.toml");
        std::fs::write(&path, config).unwrap();
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_cachet"));
        cmd.arg("--config")
            .arg(&path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        cmd.env_remove("CACHET_ADMIN_TOKEN")
            .env_remove("CACHET_CLUSTER_SECRET");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let started = loop {
            let line = lines
                .recv_timeout(Duration::from_secs(10))
                .expect("binary did not start");
            if line.contains("cachet started") {
                break line;
            }
        };
        let v: serde_json::Value = serde_json::from_str(&started).unwrap();
        let field = |name: &str| v["fields"][name].as_str().unwrap().parse().unwrap();
        Binary {
            child,
            proxy_addr: field("listen"),
            admin_addr: field("admin"),
            lines,
        }
    }

    /// Sends a signal by name through kill(1).
    pub fn signal(&self, name: &str) {
        let status = std::process::Command::new("kill")
            .arg(format!("-{name}"))
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Waits for the process to exit and returns its status.
    pub fn wait(&mut self) -> std::process::ExitStatus {
        for _ in 0..500 {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("binary did not exit");
    }

    /// Log lines printed so far.
    pub fn drain_lines(&self) -> Vec<String> {
        self.lines.try_iter().collect()
    }
}

impl Drop for Binary {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A scratch directory removed on drop.
pub struct Scratch(pub std::path::PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Scratch {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("cachet-it-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Config text for the binary on ephemeral ports.
pub fn binary_config(node_id: &str, origin: SocketAddr, extra: &str) -> String {
    format!(
        "node_id = \"{node_id}\"\nlisten = \"127.0.0.1:0\"\nadmin_listen = \"127.0.0.1:0\"\n\
         [[origins]]\nname = \"primary\"\nurl = \"http://{origin}\"\n{extra}"
    )
}

/// A server that answers every request with the given raw bytes, then
/// closes the connection.
pub async fn raw_server(response: &'static [u8]) -> (SocketAddr, JoinHandle<()>) {
    raw_sequence(vec![response]).await
}

/// Like `raw_server`, answering the n-th connection with the n-th
/// response and repeating the last one.
pub async fn raw_sequence(responses: Vec<&'static [u8]>) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut n = 0;
        while let Ok((mut s, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await;
            let response = responses[n.min(responses.len() - 1)];
            n += 1;
            let _ = s.write_all(response).await;
            let _ = s.shutdown().await;
        }
    });
    (addr, task)
}
