//! steer's `cmd/steer` tests (`main_test.go` and `tls_test.go`),
//! translated: the proxy run by `edgefront_core::reload::run` against real
//! upstreams, reloaded by SIGHUP (through `Signals::channel`) and
//! `POST /reload`, refused reloads that keep the old configuration
//! serving, TLS with certificates rotated under a live connection, and the
//! drain.

#[path = "../../edgefront-core/tests/common/tls.rs"]
mod tlsc;

use std::convert::Infallible;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use edgefront_core::reload::{self, Bound, Settings, SignalSender, Signals, Stopped};
use edgefront_proxy::server::Proxy;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// Log lines written while a test runs, shared with the subscriber.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    async fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.text().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "the log never mentioned {needle:?}:\n{}",
                self.text()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

impl Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Captures this thread's log (the tests run on a current-thread
/// runtime, so every task logs here) until the guard drops.
fn capture() -> (Logs, tracing::subscriber::DefaultGuard) {
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    (logs, tracing::subscriber::set_default(subscriber))
}

fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("edgefront-lifecycle-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An upstream that answers every request with `name`, or with the
/// request's `X-Forwarded-Proto` when `name` is `"proto"`, after `delay`.
async fn upstream(name: &'static str, delay: Duration) -> SocketAddr {
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = socket.accept().await {
            tokio::spawn(async move {
                let svc =
                    hyper::service::service_fn(move |req: http::Request<Incoming>| async move {
                        tokio::time::sleep(delay).await;
                        let body = if name == "proto" {
                            req.headers()
                                .get("x-forwarded-proto")
                                .map(|v| v.to_str().unwrap().to_owned())
                                .unwrap_or_default()
                        } else {
                            name.to_owned()
                        };
                        Ok::<_, Infallible>(http::Response::new(Full::new(Bytes::from(body))))
                    });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tcp), svc)
                    .await;
            });
        }
    });
    addr
}

/// Writes steer's test configuration: one route to one endpoint.
fn write_config(path: &Path, admin: &str, upstream: SocketAddr, extra: &str) {
    let js = format!(
        r#"{{"listen":"127.0.0.1:0","admin_listen":"{admin}","routes":[{{"cluster":"web"}}],
        "clusters":[{{"name":"web","endpoints":[{{"address":"{upstream}"}}]}}]{extra}}}"#
    );
    std::fs::write(path, js).unwrap();
}

/// Sends one request over a new connection and returns its status and
/// body, or `(0, error)` when it cannot.
async fn request(method: &str, addr: SocketAddr, path: &str, token: &str) -> (u16, String) {
    let Ok(tcp) = TcpStream::connect(addr).await else {
        return (0, "connect refused".to_owned());
    };
    let Ok((mut send, conn)) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await
    else {
        return (0, "handshake failed".to_owned());
    };
    tokio::spawn(conn);
    let mut req = http::Request::builder()
        .method(method)
        .uri(path)
        .header("host", addr.to_string());
    if !token.is_empty() {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    match send
        .send_request(req.body(Empty::<Bytes>::new()).unwrap())
        .await
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            (status, String::from_utf8_lossy(&body).into_owned())
        }
        Err(e) => (0, e.to_string()),
    }
}

async fn get(addr: SocketAddr, path: &str, token: &str) -> (u16, String) {
    request("GET", addr, path, token).await
}

/// A running process: its addresses, its signals and the task running it.
struct Process {
    bound: Bound,
    signals: SignalSender,
    task: JoinHandle<Result<Stopped, reload::ListenError>>,
}

/// Loads `path` and runs the proxy on it, as steer's `run`.
async fn start(path: &Path, settings: Settings) -> Process {
    let path = path.to_str().unwrap().to_owned();
    let loaded = reload::load(&path, &settings, "EDGEFRONT").unwrap();
    let proxy = Arc::new(Proxy::new(&loaded.config));
    let (signals, sig) = Signals::channel();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(reload::run(
        loaded,
        path,
        settings,
        "EDGEFRONT",
        proxy,
        sig,
        move |b| {
            let _ = tx.send(b);
        },
    ));
    let bound = rx.await.expect("run reports its addresses");
    Process {
        bound,
        signals,
        task,
    }
}

fn settings(token: &str, drain: u64, shutdown: u64) -> Settings {
    Settings {
        admin_token: token.to_owned(),
        drain_period: Duration::from_millis(drain),
        shutdown_timeout: Duration::from_millis(shutdown),
    }
}

const TOKEN: &str = "admin-token-0123456789";

/// steer's TestRunReloadAndDrain.
#[tokio::test]
async fn run_reload_and_drain() {
    let (logs, _guard) = capture();
    let dir = scratch("reload");
    let cfg = dir.join("steer.json");
    let a = upstream("a", Duration::ZERO).await;
    let b = upstream("b", Duration::ZERO).await;
    write_config(&cfg, "127.0.0.1:0", a, "");
    let p = start(&cfg, settings(TOKEN, 300, 2000)).await;
    let (proxy, admin) = (p.bound.proxy, p.bound.admin);

    assert_eq!(get(proxy, "/", "").await, (200, "a".to_owned()));
    assert_eq!(get(admin, "/clusters", "").await.0, 401);
    let (code, body) = get(admin, "/clusters", TOKEN).await;
    assert_eq!(code, 200);
    assert!(body.contains(&a.to_string()), "{body}");

    // SIGHUP reload switches the upstream.
    write_config(&cfg, "127.0.0.1:0", b, "");
    p.signals.hup();
    let deadline = Instant::now() + Duration::from_secs(5);
    while get(proxy, "/", "").await.1 != "b" {
        assert!(
            Instant::now() < deadline,
            "SIGHUP reload did not take effect"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // A broken file is refused over the admin API and the old config serves.
    std::fs::write(&cfg, r#"{"listen":"#).unwrap();
    let (code, body) = request("POST", admin, "/reload", TOKEN).await;
    assert_eq!(code, 400, "{body}");
    let out: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(out["error"]["code"], "invalid_config");
    p.signals.hup(); // also refused; must not stop the server
    assert_eq!(get(proxy, "/", "").await, (200, "b".to_owned()));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, metrics) = get(admin, "/metrics", TOKEN).await;
        if metrics.contains(r#"steer_config_reloads_total{result="failure"} 2"#) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "failed reloads not counted:\n{metrics}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // A reload that would move the listener is refused too.
    write_config(&cfg, "127.0.0.1:1", a, "");
    assert_eq!(request("POST", admin, "/reload", TOKEN).await.0, 400);
    // A good file through the admin API.
    write_config(&cfg, "127.0.0.1:0", a, "");
    assert_eq!(request("POST", admin, "/reload", TOKEN).await.0, 200);
    assert_eq!(get(proxy, "/", "").await, (200, "a".to_owned()));

    // Drain: readyz turns 503 while the proxy still answers, then both stop.
    p.signals.stop();
    let deadline = Instant::now() + Duration::from_secs(5);
    while get(admin, "/readyz", "").await.0 != 503 {
        assert!(Instant::now() < deadline, "readyz never reported draining");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(get(proxy, "/", "").await, (200, "a".to_owned()));
    let stopped = tokio::time::timeout(Duration::from_secs(10), p.task)
        .await
        .expect("shuts down")
        .unwrap()
        .unwrap();
    assert_eq!(stopped.proxy.forced, 0);
    assert_eq!(get(proxy, "/", "").await.0, 0, "the proxy still listens");
    let text = logs.text();
    for want in [
        r#""message":"listening""#,
        r#""message":"reloaded on SIGHUP""#,
        r#""message":"configuration rejected; the previous configuration keeps serving""#,
        "listen and admin_listen cannot change on reload",
        r#""message":"draining""#,
        r#""message":"shutting down""#,
        r#""message":"stopped""#,
    ] {
        assert!(text.contains(want), "the log lacks {want}:\n{text}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// steer's TestStartupErrors: what refuses to start, and with what.
#[tokio::test]
async fn startup_errors() {
    let dir = scratch("startup");
    let good = dir.join("good.json");
    write_config(&good, "127.0.0.1:0", "127.0.0.1:1".parse().unwrap(), "");
    let public = dir.join("public.json");
    write_config(&public, "0.0.0.0:0", "127.0.0.1:1".parse().unwrap(), "");
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy_addr = busy.local_addr().unwrap().to_string();
    let clash = dir.join("clash.json");
    std::fs::write(
        &clash,
        format!(
            r#"{{"listen":"{busy_addr}","routes":[{{"cluster":"w"}}],"clusters":[{{"name":"w","endpoints":[{{"address":"h:1"}}]}}]}}"#
        ),
    )
    .unwrap();
    let admin_clash = dir.join("adminclash.json");
    write_config(&admin_clash, &busy_addr, "127.0.0.1:1".parse().unwrap(), "");

    let none = settings("", 0, 1000);
    let load = |p: &Path, s: &Settings| reload::load(p.to_str().unwrap(), s, "EDGEFRONT");
    let missing = load(&dir.join("missing.json"), &none).unwrap_err();
    assert!(missing.contains("missing.json"), "{missing}");
    let err = load(&public, &none).unwrap_err();
    assert!(err.contains("not a loopback address"), "{err}");
    assert!(err.contains("EDGEFRONT_ADMIN_TOKEN"), "{err}");
    for (file, listen) in [(&clash, &busy_addr), (&admin_clash, &busy_addr)] {
        let loaded = load(file, &none).unwrap();
        let proxy = Arc::new(Proxy::new(&loaded.config));
        let (_signals, sig) = Signals::channel();
        let err = reload::run(
            loaded,
            file.to_str().unwrap().to_owned(),
            none.clone(),
            "EDGEFRONT",
            proxy,
            sig,
            |_| panic!("nothing listens"),
        )
        .await
        .unwrap_err();
        assert_eq!(&err.listen, listen);
        assert_eq!(err.error.kind(), std::io::ErrorKind::AddrInUse, "{err}");
        assert!(
            err.to_string()
                .starts_with(&format!("cannot listen on {listen}: "))
        );
    }
    // A public admin address is fine with a token.
    let p = start(&public, settings(TOKEN, 0, 1000)).await;
    p.signals.stop();
    p.task.await.unwrap().unwrap();
    // And the good file starts and stops, with the signal sender dropped
    // standing in for SIGTERM.
    let p = start(&good, none).await;
    drop(p.signals);
    p.task.await.unwrap().unwrap();
    drop(busy);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The shutdown timeout cuts a request still in flight once the drain is
/// over, and the forced close is logged.
#[tokio::test]
async fn shutdown_deadline_cuts_requests_in_flight() {
    let (logs, _guard) = capture();
    let dir = scratch("deadline");
    let cfg = dir.join("steer.json");
    let slow = upstream("slow", Duration::from_secs(30)).await;
    write_config(&cfg, "127.0.0.1:0", slow, "");
    let p = start(&cfg, settings("", 0, 200)).await;
    let proxy = p.bound.proxy;
    let inflight = tokio::spawn(async move { get(proxy, "/", "").await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let start = Instant::now();
    p.signals.stop();
    let stopped = p.task.await.unwrap().unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(stopped.proxy.forced, 1);
    assert_eq!(inflight.await.unwrap().0, 0, "the request was cut");
    logs.wait_for("shutdown timeout reached; closing remaining connections")
        .await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Copies a fixture pair into `dir` as `<base>.crt` and `<base>.key`.
fn copy_pair(dir: &Path, fixture: &str, base: &str) {
    for ext in ["crt", "key"] {
        std::fs::copy(
            tlsc::fixture(&format!("{fixture}.{ext}")),
            dir.join(format!("{base}.{ext}")),
        )
        .unwrap();
    }
}

/// steer's TestRunTLS.
#[tokio::test]
async fn run_tls() {
    let (logs, _guard) = capture();
    let dir = scratch("tls");
    copy_pair(&dir, "exact", "site");
    let up = upstream("proto", Duration::ZERO).await;
    let cfg = dir.join("steer.json");
    // Relative paths resolve against the configuration file's directory.
    write_config(
        &cfg,
        "127.0.0.1:0",
        up,
        r#","tls":{"certificates":[{"cert_file":"site.crt","key_file":"site.key"}],"handshake_timeout":"1s"}"#,
    );
    let p = start(&cfg, settings("", 0, 2000)).await;
    let proxy = p.bound.proxy;
    let opts = tlsc::ClientOpts::default();

    let resuming = tlsc::client_config(&opts);
    assert!(!tlsc::resumed(proxy, "api.example.com", &resuming).await);
    let mut live = tlsc::Client::connect(proxy, "api.example.com", &opts).await;
    assert_eq!(live.cert, "exact");
    assert_eq!(live.get("/").await.unwrap(), (200, "https".to_owned()));
    // Plain HTTP to the TLS listener fails the handshake and is not
    // proxied.
    assert_ne!(get(proxy, "/", "").await.0, 200);

    // Rotate the certificate files and reload with SIGHUP.
    copy_pair(&dir, "rotated", "rotated");
    write_config(
        &cfg,
        "127.0.0.1:0",
        up,
        r#","tls":{"certificates":[{"cert_file":"rotated.crt","key_file":"rotated.key"}]}"#,
    );
    p.signals.hup();
    logs.wait_for(r#""message":"reloaded on SIGHUP""#).await;
    // The session ticket keys outlive the reload: the ticket from before
    // it resumes.
    assert!(tlsc::resumed(proxy, "api.example.com", &resuming).await);
    assert_eq!(live.get("/").await.unwrap(), (200, "https".to_owned()));
    assert!(live.is_open(), "the live connection carries on");
    let fresh = tlsc::Client::connect(proxy, "api.example.com", &opts).await;
    assert_eq!(fresh.cert, "rotated");

    // Reloads that would turn TLS off or point at a missing key are
    // refused, and the rotated certificate keeps serving.
    write_config(&cfg, "127.0.0.1:0", up, "");
    p.signals.hup();
    logs.wait_for("tls cannot be turned on or off").await;
    write_config(
        &cfg,
        "127.0.0.1:0",
        up,
        r#","tls":{"certificates":[{"cert_file":"rotated.crt","key_file":"missing.key"}]}"#,
    );
    p.signals.hup();
    logs.wait_for("missing.key").await;
    let after = tlsc::Client::connect(proxy, "api.example.com", &opts).await;
    assert_eq!(after.cert, "rotated");

    // The handshake series are on /metrics.
    let (_, metrics) = get(p.bound.admin, "/metrics", "").await;
    assert!(
        metrics.contains(r#"steer_tls_handshakes_total{result="ok"} 5"#),
        "{metrics}"
    );
    assert!(
        metrics.contains(r#"steer_tls_handshakes_total{result="error"} 1"#),
        "{metrics}"
    );
    assert!(
        metrics.contains("steer_tls_handshakes_in_progress 0"),
        "{metrics}"
    );
    assert!(logs.text().contains(r#""tls":true"#));
    p.signals.stop();
    p.task.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// steer's TestStartupTLSError, and a certificate that serves with a
/// warning.
#[tokio::test]
async fn startup_tls_error() {
    let (logs, _guard) = capture();
    let dir = scratch("tlserror");
    let cfg = dir.join("steer.json");
    write_config(
        &cfg,
        "127.0.0.1:0",
        "127.0.0.1:1".parse().unwrap(),
        r#","tls":{"certificates":[{"cert_file":"none.crt","key_file":"none.key"}]}"#,
    );
    let err = reload::load(cfg.to_str().unwrap(), &settings("", 0, 0), "EDGEFRONT").unwrap_err();
    assert!(err.contains("none.crt"), "{err}");
    assert!(err.starts_with(cfg.to_str().unwrap()), "{err}");

    copy_pair(&dir, "expired", "old");
    write_config(
        &cfg,
        "127.0.0.1:0",
        "127.0.0.1:1".parse().unwrap(),
        r#","tls":{"certificates":[{"cert_file":"old.crt","key_file":"old.key"}]}"#,
    );
    reload::load(cfg.to_str().unwrap(), &settings("", 0, 0), "EDGEFRONT").unwrap();
    logs.wait_for("old.crt expired at 2020-01-03T00:00:00Z")
        .await;
    assert!(logs.text().contains(r#""message":"tls certificate""#));
    let _ = std::fs::remove_dir_all(&dir);
}
