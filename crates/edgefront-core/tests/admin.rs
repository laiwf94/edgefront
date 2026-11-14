//! The admin router on the core listener, exercised over real TCP
//! connections with hand-written HTTP/1.1.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use edgefront_core::admin::{BearerToken, Router, json, read_body};
use edgefront_core::body::text;
use edgefront_core::listener::{ServerLimits, serve};
use http::{Method, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

struct State {
    token: BearerToken,
    calls: AtomicUsize,
}

fn router() -> Router<State> {
    let state = Arc::new(State {
        token: BearerToken::new("secret"),
        calls: AtomicUsize::new(0),
    });
    Router::new(state)
        .health()
        .route(Method::POST, "/echo", |s: Arc<State>, req, _| async move {
            s.calls.fetch_add(1, Ordering::Relaxed);
            if let Err(refusal) = s.token.check(req.headers()) {
                return refusal.into_response();
            }
            match read_body(req.into_body(), 16).await {
                Ok(body) => json(StatusCode::OK, &serde_json::json!({ "len": body.len() })),
                Err(refusal) => refusal.into_response(),
            }
        })
        .any("/any", |_, req, peer: SocketAddr| async move {
            let msg: &'static str = if req.method() == Method::DELETE {
                "delete\n"
            } else {
                "other\n"
            };
            assert!(peer.ip().is_loopback());
            text(StatusCode::OK, msg)
        })
        .route(Method::GET, "/slow", |_, _, _| async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            text(StatusCode::OK, "slow\n")
        })
        .route(Method::GET, "/hang", |_, _, _| async {
            std::future::pending::<()>().await;
            unreachable!()
        })
}

const LIMITS: ServerLimits = ServerLimits {
    header_read_timeout: Duration::from_millis(300),
    idle_timeout: Duration::from_millis(300),
    max_header_bytes: 8192,
    max_headers: 100,
    drain_timeout: Duration::from_secs(2),
};

struct Server {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Server {
    async fn start(limits: ServerLimits) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            serve(listener, Arc::new(router()), limits, async {
                let _ = rx.await;
            })
            .await;
        });
        Server {
            addr,
            stop: Some(tx),
            task,
        }
    }

    async fn shutdown(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.await.unwrap();
    }
}

/// Sends `raw` on a fresh connection and reads until the server closes it.
async fn exchange(addr: SocketAddr, raw: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(raw).await.unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    String::from_utf8_lossy(&out).into_owned()
}

fn get(path: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nhost: a\r\nconnection: close\r\n\r\n").into_bytes()
}

fn post(path: &str, auth: Option<&str>, body: &str) -> Vec<u8> {
    let auth = auth
        .map(|t| format!("authorization: {t}\r\n"))
        .unwrap_or_default();
    format!(
        "POST {path} HTTP/1.1\r\nhost: a\r\nconnection: close\r\n{auth}content-length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn status(resp: &str) -> u16 {
    resp.split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn body(resp: &str) -> &str {
    resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
}

#[tokio::test]
async fn routes_by_method_and_exact_path() {
    let server = Server::start(LIMITS).await;
    let a = server.addr;

    let r = exchange(a, &get("/healthz")).await;
    assert_eq!(status(&r), 200, "{r}");
    assert_eq!(body(&r), "ok\n");
    assert!(r.to_ascii_lowercase().contains("cache-control: no-store"));

    let r = exchange(a, &post("/healthz", None, "")).await;
    assert_eq!(status(&r), 405);
    assert_eq!(body(&r), r#"{"error":"method not allowed"}"#);
    assert!(r.contains("application/json"));

    for path in ["/nope", "/healthz/", "/heal"] {
        let r = exchange(a, &get(path)).await;
        assert_eq!(status(&r), 404, "{path}");
        assert_eq!(body(&r), r#"{"error":"not found"}"#);
    }
    // The query is not part of the path.
    assert_eq!(status(&exchange(a, &get("/healthz?x=1")).await), 200);

    let r = exchange(
        a,
        b"DELETE /any HTTP/1.1\r\nhost: a\r\nconnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(body(&r), "delete\n");
    assert_eq!(body(&exchange(a, &get("/any")).await), "other\n");
    server.shutdown().await;
}

#[tokio::test]
async fn bearer_tokens_and_body_limits() {
    let server = Server::start(LIMITS).await;
    let a = server.addr;

    let r = exchange(a, &post("/echo", None, "x")).await;
    assert_eq!(status(&r), 401);
    assert!(r.to_ascii_lowercase().contains("www-authenticate: bearer"));
    assert_eq!(body(&r), r#"{"error":"missing or wrong bearer token"}"#);
    let r = exchange(a, &post("/echo", Some("Bearer wrong"), "x")).await;
    assert_eq!(status(&r), 401);

    let r = exchange(a, &post("/echo", Some("Bearer secret"), "0123456789abcdef")).await;
    assert_eq!(status(&r), 200, "{r}");
    assert_eq!(body(&r), r#"{"len":16}"#);

    let r = exchange(
        a,
        &post("/echo", Some("Bearer secret"), "0123456789abcdefg"),
    )
    .await;
    assert_eq!(status(&r), 413);
    assert_eq!(body(&r), r#"{"error":"body too large"}"#);

    // A body cut short by the client cannot be read.
    let mut s = TcpStream::connect(a).await.unwrap();
    s.write_all(b"POST /echo HTTP/1.1\r\nhost: a\r\nauthorization: Bearer secret\r\ncontent-length: 10\r\n\r\nabc")
        .await
        .unwrap();
    s.shutdown().await.unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    let r = String::from_utf8_lossy(&out);
    assert!(r.is_empty() || status(&r) == 400, "{r}");

    server.shutdown().await;
}

#[tokio::test]
async fn slow_and_oversized_heads_are_refused() {
    let server = Server::start(LIMITS).await;
    let a = server.addr;

    // Half a head, then silence: the listener closes the connection
    // without a response after the header read timeout.
    let started = Instant::now();
    let r = exchange(a, b"GET /healthz HTTP/1.1\r\nhost").await;
    let waited = started.elapsed();
    assert_eq!(r, "");
    assert!(waited >= Duration::from_millis(250), "{waited:?}");
    assert!(waited < Duration::from_secs(5), "{waited:?}");

    let big = format!(
        "GET /healthz HTTP/1.1\r\nhost: a\r\nx: {}\r\n\r\n",
        "y".repeat(16 << 10)
    );
    let r = exchange(a, big.as_bytes()).await;
    assert_eq!(status(&r), 431, "{r}");
    server.shutdown().await;
}

#[tokio::test]
async fn shutdown_lets_open_requests_finish() {
    let server = Server::start(LIMITS).await;
    let a = server.addr;
    let slow = tokio::spawn(async move { exchange(a, &get("/slow")).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    server.shutdown().await;
    assert!(started.elapsed() < Duration::from_secs(2));
    let r = slow.await.unwrap();
    assert_eq!(status(&r), 200);
    assert_eq!(body(&r), "slow\n");
    // Nothing is accepted any more.
    assert!(TcpStream::connect(a).await.is_err());
}

#[tokio::test]
async fn shutdown_gives_up_after_the_drain_timeout() {
    let limits = ServerLimits {
        drain_timeout: Duration::from_millis(200),
        ..LIMITS
    };
    let server = Server::start(limits).await;
    let a = server.addr;
    let hung = tokio::spawn(async move { exchange(a, &get("/hang")).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    server.shutdown().await;
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(150), "{waited:?}");
    assert!(waited < Duration::from_secs(2), "{waited:?}");
    drop(hung);
}

mod api {
    //! steer's operator API ([`Api`]) on the core listener. The answers
    //! themselves are diffed against steer in `admin_steer.rs`; this
    //! covers what only a real connection shows, and concurrent use.

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    use edgefront_core::admin::go_json::unix;
    use edgefront_core::admin::{Api, Control, ProxyState};

    use super::*;

    const TOKEN: &str = "0123456789abcdef";

    struct Live {
        draining: AtomicBool,
        version: AtomicU64,
        loaded: Mutex<u64>,
    }

    impl Control for Live {
        fn ready(&self) -> bool {
            !self.draining.load(Ordering::SeqCst)
        }

        fn state(&self) -> ProxyState {
            ProxyState {
                config_version: self.version.load(Ordering::SeqCst),
                loaded_at: unix(*self.loaded.lock().unwrap(), 0),
                clusters: vec![],
            }
        }

        async fn reload(&self) -> Result<u64, String> {
            tokio::task::yield_now().await;
            let v = self.version.fetch_add(1, Ordering::SeqCst) + 1;
            *self.loaded.lock().unwrap() = v;
            Ok(v)
        }

        fn metrics(&self, out: &mut String) {
            out.push_str("steer_up 1\n");
        }
    }

    async fn start() -> (SocketAddr, Arc<Live>, oneshot::Sender<()>, JoinHandle<()>) {
        let live = Arc::new(Live {
            draining: AtomicBool::new(false),
            version: AtomicU64::new(1),
            loaded: Mutex::new(0),
        });
        let api = Arc::new(Api::new(live.clone(), TOKEN).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            serve(listener, api, LIMITS, async {
                let _ = rx.await;
            })
            .await;
        });
        (addr, live, tx, task)
    }

    fn header<'a>(resp: &'a str, name: &str) -> Option<&'a str> {
        let head = resp.split_once("\r\n\r\n").map_or(resp, |(h, _)| h);
        head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }

    #[tokio::test]
    async fn routes_over_tcp() {
        let (a, live, stop, task) = start().await;
        let bearer = format!("Bearer {TOKEN}");

        let r = exchange(a, &get("/healthz")).await;
        assert_eq!(status(&r), 200, "{r}");
        assert_eq!(body(&r), "{\n  \"status\": \"ok\"\n}\n");
        assert_eq!(header(&r, "x-content-type-options"), Some("nosniff"));
        assert_eq!(header(&r, "x-request-id").map(str::len), Some(24));

        let r = exchange(a, &get("/clusters")).await;
        assert_eq!(status(&r), 401);
        assert_eq!(
            header(&r, "www-authenticate"),
            Some("Bearer realm=\"steer-admin\"")
        );

        let r = exchange(a, &post("/reload", Some(&bearer), "ignored")).await;
        assert_eq!(status(&r), 200, "{r}");
        assert_eq!(
            body(&r),
            "{\n  \"config_version\": 2,\n  \"status\": \"reloaded\"\n}\n"
        );
        let r = exchange(
            a,
            format!("GET /clusters HTTP/1.1\r\nhost: a\r\nconnection: close\r\nauthorization: {bearer}\r\n\r\n")
                .as_bytes(),
        )
        .await;
        assert_eq!(
            body(&r),
            "{\n  \"config_version\": 2,\n  \"loaded_at\": \"1970-01-01T00:00:02Z\",\n  \"clusters\": null\n}\n"
        );

        live.draining.store(true, Ordering::SeqCst);
        let r = exchange(a, &get("/readyz")).await;
        assert_eq!(status(&r), 503);
        assert_eq!(body(&r), "{\n  \"status\": \"draining\"\n}\n");

        // What Go's server answers before any handler.
        let r = exchange(a, &get("/%zz")).await;
        assert_eq!(status(&r), 400, "{r}");
        assert_eq!(body(&r), "400 Bad Request");
        assert_eq!(header(&r, "connection"), Some("close"));
        let r = exchange(
            a,
            b"OPTIONS * HTTP/1.1\r\nhost: a\r\nconnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status(&r), 200, "{r}");
        assert_eq!(header(&r, "content-length"), Some("0"));

        // A HEAD gets the head of the 405 and no body.
        let r = exchange(
            a,
            b"HEAD /readyz HTTP/1.1\r\nhost: a\r\nconnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status(&r), 405);
        assert_eq!(header(&r, "allow"), Some("GET"));
        assert_eq!(body(&r), "");

        let r = exchange(a, &get("//metrics?a=b")).await;
        assert_eq!(status(&r), 307);
        assert_eq!(header(&r, "location"), Some("/metrics?a=b"));

        let _ = stop.send(());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_reloads_each_get_their_own_version() {
        let (a, live, stop, task) = start().await;
        let bearer = format!("Bearer {TOKEN}");
        let mut calls = Vec::new();
        for _ in 0..16 {
            let req = post("/reload", Some(&bearer), "");
            calls.push(tokio::spawn(async move { exchange(a, &req).await }));
        }
        let mut versions = Vec::new();
        for c in calls {
            let r = c.await.unwrap();
            assert_eq!(status(&r), 200, "{r}");
            let v: serde_json::Value = serde_json::from_str(body(&r)).unwrap();
            versions.push(v["config_version"].as_u64().unwrap());
        }
        versions.sort_unstable();
        assert_eq!(versions, (2..18).collect::<Vec<u64>>());
        assert_eq!(live.version.load(Ordering::SeqCst), 17);
        let _ = stop.send(());
        task.await.unwrap();
    }
}
