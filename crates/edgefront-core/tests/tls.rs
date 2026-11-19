//! steer's `internal/tlsedge` tests, translated: SNI selection, warnings,
//! load errors, and real handshakes over loopback through
//! `listener::serve_tls`, with the certificates steer's tests issued.

#[path = "common/tls.rs"]
mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use common::*;
use edgefront_core::config::TlsVersion;
use edgefront_core::listener::{Handler, ServerLimits, serve_tls};
use edgefront_core::tls::{
    Acceptor, HandshakeResult, OnHandshake, Snapshot, Store, TlsInfo, UnknownServerName,
};
use http::{Request, Response};
use http_body_util::Full;
use hyper::body::Incoming;
use rustls::crypto::ring::cipher_suite as suites;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

fn load(tls: &edgefront_core::config::Tls) -> Arc<Snapshot> {
    Arc::new(Snapshot::load(tls).unwrap())
}

/// The fixture a name selects.
fn selected(s: &Snapshot, sni: &str) -> Result<&'static str, UnknownServerName> {
    let i = s.select(sni)?;
    Ok(which(&leaf_by_file(&s.certificates()[i].cert_file)))
}

fn leaf_by_file(path: &str) -> rustls::pki_types::CertificateDer<'static> {
    use rustls::pki_types::pem::PemObject;
    rustls::pki_types::CertificateDer::from_pem_file(path).unwrap()
}

#[test]
fn select() {
    let s = load(&standard());
    for (sni, want) in [
        ("api.example.com", "exact"),
        ("API.Example.COM.", "exact"),
        ("www.example.com", "wild"),
        ("example.com", "wild"),
        ("a.b.example.com", "default"),
        ("example.com.evil", "default"),
        (".example.com", "default"),
        ("", "default"),
        ("unknown.test", "default"),
        ("default.test", "default"),
        ("dual.test", "dual-ecdsa"),
    ] {
        assert_eq!(selected(&s, sni), Ok(want), "{sni:?}");
    }

    let mut strict = standard();
    strict.strict_sni = true;
    let s = load(&strict);
    for sni in ["", "a.b.example.com", "unknown.test"] {
        assert_eq!(selected(&s, sni), Err(UnknownServerName), "{sni:?}");
    }
    assert_eq!(selected(&s, "default.test"), Ok("default"));
}

#[test]
fn server_names_override() {
    let s = load(&tls_config(vec![
        cert("default", &[]),
        cert("wild", &["*.example.org"]),
    ]));
    assert_eq!(selected(&s, "www.example.com"), Ok("default"));
    assert_eq!(selected(&s, "www.example.org"), Ok("wild"));
    let w = s.warnings(SystemTime::now());
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(
        w[0].contains(r#"does not cover server name "*.example.org""#),
        "{w:?}"
    );
    assert_eq!(s.certificates()[1].names, vec!["*.example.org".to_owned()]);
}

#[test]
fn warnings_validity() {
    let s = load(&tls_config(vec![cert("expired", &[]), cert("future", &[])]));
    let w = s.warnings(SystemTime::now()).join("\n");
    assert!(
        w.contains("expired.crt expired at 2020-01-03T00:00:00Z"),
        "{w}"
    );
    assert!(
        w.contains("future.crt is not valid until 2120-01-01T00:00:00Z"),
        "{w}"
    );
    assert_eq!(s.certificates().len(), 2);
    // The certificate's own names, deduplicated and normalized.
    assert_eq!(s.certificates()[0].names, vec!["old.test".to_owned()]);
    let good = load(&standard());
    assert!(good.warnings(SystemTime::now()).is_empty());
    assert_eq!(
        good.certificates()[2].names,
        vec!["*.example.com".to_owned(), "example.com".to_owned()]
    );
}

#[test]
fn load_errors() {
    let dir = std::env::temp_dir().join(format!("edgefront-tls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let big = dir.join("big.pem");
    std::fs::write(&big, vec![b'a'; (1 << 20) + 1]).unwrap();
    let big = big.to_string_lossy().into_owned();
    let with = |c: &str, k: &str| edgefront_core::config::Certificate {
        cert_file: c.to_owned(),
        key_file: k.to_owned(),
        server_names: None,
    };
    let cases = [
        ("no certs", tls_config(vec![]), "at least one certificate"),
        (
            "missing file",
            tls_config(vec![with(&fixture("absent.crt"), &fixture("default.key"))]),
            "open ",
        ),
        (
            "garbage",
            tls_config(vec![with(&fixture("garbage.pem"), &fixture("default.key"))]),
            "failed to find any PEM data in certificate input",
        ),
        (
            "garbage key",
            tls_config(vec![with(&fixture("default.crt"), &fixture("garbage.pem"))]),
            "failed to find any PEM data in key input",
        ),
        (
            "missing key",
            tls_config(vec![with(&fixture("default.crt"), &fixture("missing.key"))]),
            "missing.key",
        ),
        (
            "key mismatch",
            tls_config(vec![with(&fixture("default.crt"), &fixture("exact.key"))]),
            "default.crt and",
        ),
        (
            "later no names",
            tls_config(vec![cert("default", &[]), cert("nonames", &[])]),
            "has no DNS names; set server_names",
        ),
        (
            "too large",
            tls_config(vec![with(&big, &fixture("default.key"))]),
            "larger than 1048576 bytes",
        ),
    ];
    for (name, c, want) in cases {
        let err = Snapshot::load(&c).unwrap_err().to_string();
        assert!(err.contains(want), "{name}: {err}");
        if name != "no certs" {
            assert!(
                err.starts_with("tls.certificates[0]") || err.starts_with("tls.certificates[1]"),
                "{name}: {err}"
            );
        }
    }
    // A first certificate without names is fine: it is the fallback.
    let s = load(&tls_config(vec![cert("nonames", &[])]));
    assert!(s.certificates()[0].names.is_empty());
    // A suite list of only CBC suites leaves nothing rustls can offer.
    let mut cbc = standard();
    cbc.cipher_suites = Some(vec![suite(0xc009), suite(0xc013)]);
    assert!(
        Snapshot::load(&cbc)
            .unwrap_err()
            .to_string()
            .contains("AES-CBC suites are not supported")
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Answers every request with the SNI the connection's handshake carried.
struct Sni;

impl Handler for Sni {
    type Body = Full<Bytes>;

    async fn call(self: Arc<Self>, req: Request<Incoming>, _: SocketAddr) -> Response<Full<Bytes>> {
        let name = req
            .extensions()
            .get::<Arc<TlsInfo>>()
            .map(|i| i.server_name.clone().unwrap_or_default())
            .unwrap_or_else(|| "no tls info".to_owned());
        Response::new(Full::new(Bytes::from(name)))
    }
}

/// A running TLS listener.
struct Server {
    addr: SocketAddr,
    acceptor: Arc<Acceptor>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<edgefront_core::listener::ServeReport>>,
}

impl Server {
    async fn start(acceptor: Arc<Acceptor>) -> Server {
        let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(serve_tls(
            socket,
            Arc::clone(&acceptor),
            Arc::new(Sni),
            ServerLimits {
                drain_timeout: Duration::from_secs(2),
                ..ServerLimits::default()
            },
            async move {
                let _ = stopped.await;
            },
        ));
        Server {
            addr,
            acceptor,
            stop: Some(stop),
            task: Some(task),
        }
    }

    async fn shutdown(mut self) -> edgefront_core::listener::ServeReport {
        let _ = self.stop.take().unwrap().send(());
        self.task.take().unwrap().await.unwrap()
    }
}

fn counting() -> (OnHandshake, Arc<Mutex<Vec<HandshakeResult>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = Arc::clone(&seen);
    (Arc::new(move |r, _| s.lock().unwrap().push(r)), seen)
}

async fn wait_for(cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "condition not met within 5s");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn handshake_matrix() {
    let server = Server::start(Acceptor::new(Store::new(load(&standard())), None)).await;
    let h2_first = ClientOpts {
        alpn: vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        ..ClientOpts::default()
    };
    for (sni, want) in [
        ("api.example.com", "exact"),
        ("www.example.com", "wild"),
        ("default.test", "default"),
    ] {
        let s = handshake(server.addr, sni, &h2_first).await.unwrap();
        assert_eq!(presented(&s), want, "{sni}");
        let conn = s.get_ref().1;
        assert_eq!(
            conn.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_3)
        );
        assert_eq!(conn.alpn_protocol(), Some(&b"http/1.1"[..]));
    }
    // A client that only takes RSA suites gets the RSA certificate of a
    // dual pair.
    let rsa_only = ClientOpts {
        max_tls12: true,
        tls12_suites: Some(vec![suites::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256]),
        ..ClientOpts::default()
    };
    let s = handshake(server.addr, "dual.test", &rsa_only)
        .await
        .unwrap();
    assert_eq!(presented(&s), "dual-rsa");
    // An ECDSA-capable TLS 1.2 client gets the first, ECDSA one.
    let tls12 = ClientOpts {
        max_tls12: true,
        ..ClientOpts::default()
    };
    let s = handshake(server.addr, "dual.test", &tls12).await.unwrap();
    assert_eq!(presented(&s), "dual-ecdsa");
    // Unknown names fall back to the default, which the client rejects.
    assert!(
        handshake(server.addr, "a.b.example.com", &ClientOpts::default())
            .await
            .is_err()
    );
    // TLS 1.2 still works with the modern policy.
    let s = handshake(server.addr, "api.example.com", &tls12)
        .await
        .unwrap();
    assert_eq!(
        s.get_ref().1.protocol_version(),
        Some(rustls::ProtocolVersion::TLSv1_2)
    );
    // No SNI gets the default certificate.
    let any = ClientOpts {
        roots: None,
        ..ClientOpts::default()
    };
    let s = handshake(server.addr, "", &any).await.unwrap();
    assert_eq!(presented(&s), "default");
    // Requests carry the connection's TLS details.
    let mut c = Client::connect(server.addr, "www.example.com", &ClientOpts::default()).await;
    assert_eq!(c.cert, "wild");
    assert_eq!(
        c.get("/").await.unwrap(),
        (200, "www.example.com".to_owned())
    );
    server.shutdown().await;
}

#[tokio::test]
async fn version_and_cipher_policy() {
    let mut only13 = standard();
    only13.min_version = TlsVersion::Tls13;
    only13.cipher_suites = None;
    let server = Server::start(Acceptor::new(Store::new(load(&only13)), None)).await;
    let tls12 = ClientOpts {
        max_tls12: true,
        ..ClientOpts::default()
    };
    assert!(
        handshake(server.addr, "api.example.com", &tls12)
            .await
            .is_err()
    );
    handshake(server.addr, "api.example.com", &ClientOpts::default())
        .await
        .unwrap();

    // A suite the policy leaves out is refused. The policies are swapped
    // in on the same listener, as a reload would.
    let chacha_only = ClientOpts {
        max_tls12: true,
        tls12_suites: Some(vec![suites::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256]),
        ..ClientOpts::default()
    };
    let mut gcm = standard();
    gcm.cipher_suites = Some(vec![suite(0xc02b)]);
    server.acceptor.store().swap(load(&gcm));
    assert!(
        handshake(server.addr, "api.example.com", &chacha_only)
            .await
            .is_err()
    );
    let mut both = standard();
    both.cipher_suites = Some(vec![suite(0xc02b), suite(0xcca9)]);
    server.acceptor.store().swap(load(&both));
    let s = handshake(server.addr, "api.example.com", &chacha_only)
        .await
        .unwrap();
    assert_eq!(
        s.get_ref().1.negotiated_cipher_suite().map(|s| s.suite()),
        Some(rustls::CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256)
    );

    // steer's compatible policy adds AES-CBC suites, which rustls does not
    // implement: they are dropped with a warning, and the rest serve.
    let mut compatible = standard();
    compatible.cipher_suites = Some(vec![suite(0xc02b), suite(0xc009), suite(0xc013)]);
    let s = load(&compatible);
    let w = s.warnings(SystemTime::now());
    assert_eq!(
        w,
        vec![
            "cipher suite TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA is not offered: AES-CBC suites are not supported",
            "cipher suite TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA is not offered: AES-CBC suites are not supported",
        ]
    );
    server.acceptor.store().swap(s);
    handshake(server.addr, "api.example.com", &tls12)
        .await
        .unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn strict_sni_handshake() {
    let mut c = standard();
    c.strict_sni = true;
    let (on, seen) = counting();
    let server = Server::start(Acceptor::new(Store::new(load(&c)), Some(on))).await;
    let any = ClientOpts {
        roots: None,
        ..ClientOpts::default()
    };
    assert!(handshake(server.addr, "unknown.test", &any).await.is_err());
    assert!(handshake(server.addr, "", &any).await.is_err());
    handshake(server.addr, "api.example.com", &ClientOpts::default())
        .await
        .unwrap();
    let count = |r| seen.lock().unwrap().iter().filter(|&&x| x == r).count();
    wait_for(|| count(HandshakeResult::Error) == 2 && count(HandshakeResult::Ok) == 1).await;
    server.shutdown().await;
}

/// A client that connects and sends nothing is cut off at the handshake
/// timeout, and does not delay other clients.
#[tokio::test]
async fn handshake_timeout() {
    let mut c = standard();
    c.handshake_timeout = Duration::from_millis(300);
    let timeouts = Arc::new(AtomicUsize::new(0));
    let t = Arc::clone(&timeouts);
    let on: OnHandshake = Arc::new(move |r, d| {
        if r == HandshakeResult::Timeout {
            assert!(d >= Duration::from_millis(300), "{d:?}");
            t.fetch_add(1, Ordering::Relaxed);
        }
    });
    let server = Server::start(Acceptor::new(Store::new(load(&c)), Some(on))).await;
    let mut stalled = Vec::new();
    for _ in 0..8 {
        stalled.push(TcpStream::connect(server.addr).await.unwrap());
    }
    wait_for(|| server.acceptor.in_progress() == 8).await;
    // A real client gets through at once while the stalled ones wait.
    let start = Instant::now();
    handshake(server.addr, "api.example.com", &ClientOpts::default())
        .await
        .unwrap();
    assert!(
        start.elapsed() < Duration::from_millis(250),
        "{:?}",
        start.elapsed()
    );
    // Each stalled connection is closed by the server at the timeout.
    for mut raw in stalled {
        let read = tokio::time::timeout(Duration::from_secs(5), raw.read(&mut [0u8; 1]))
            .await
            .expect("the server closes a stalled connection");
        assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    }
    assert!(
        start.elapsed() >= Duration::from_millis(250),
        "{:?}",
        start.elapsed()
    );
    wait_for(|| timeouts.load(Ordering::Relaxed) == 8).await;
    wait_for(|| server.acceptor.in_progress() == 0).await;
    server.shutdown().await;
}

/// Once the limit is reached new connections wait for a slot instead of
/// starting a handshake.
#[tokio::test]
async fn max_concurrent_handshakes() {
    let mut c = standard();
    c.handshake_timeout = Duration::from_millis(400);
    c.max_concurrent_handshakes = 2;
    let server = Server::start(Acceptor::new(Store::new(load(&c)), None)).await;
    let _a = TcpStream::connect(server.addr).await.unwrap();
    let _b = TcpStream::connect(server.addr).await.unwrap();
    wait_for(|| server.acceptor.in_progress() == 2).await;
    let start = Instant::now();
    handshake(server.addr, "api.example.com", &ClientOpts::default())
        .await
        .unwrap();
    assert!(
        start.elapsed() >= Duration::from_millis(300),
        "the third handshake finished after {:?}; it should have waited for a stalled one to time out",
        start.elapsed()
    );
    server.shutdown().await;
}

/// Shutdown abandons a handshake in progress: its connection is closed,
/// and it is not reported.
#[tokio::test]
async fn shutdown_abandons_handshakes() {
    let (on, seen) = counting();
    let server = Server::start(Acceptor::new(Store::new(load(&standard())), Some(on))).await;
    let mut raw = TcpStream::connect(server.addr).await.unwrap();
    wait_for(|| server.acceptor.in_progress() == 1).await;
    let acceptor = Arc::clone(&server.acceptor);
    let report = server.shutdown().await;
    assert_eq!(report.accepted, 1);
    assert_eq!(report.forced, 0);
    let read = tokio::time::timeout(Duration::from_secs(5), raw.read(&mut [0u8; 1]))
        .await
        .expect("the pending handshake's connection is closed");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    assert_eq!(acceptor.in_progress(), 0);
    assert!(seen.lock().unwrap().is_empty());
}

/// Plain HTTP to the TLS listener fails the handshake and is not served.
#[tokio::test]
async fn plain_http_is_refused() {
    let (on, seen) = counting();
    let server = Server::start(Acceptor::new(Store::new(load(&standard())), Some(on))).await;
    let mut raw = TcpStream::connect(server.addr).await.unwrap();
    use tokio::io::AsyncWriteExt;
    raw.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), raw.read_to_end(&mut buf)).await;
    assert!(
        !String::from_utf8_lossy(&buf).contains("HTTP/1.1 200"),
        "{buf:?}"
    );
    wait_for(|| seen.lock().unwrap().as_slice() == [HandshakeResult::Error]).await;
    server.shutdown().await;
}

/// Swapping the certificate under a keep-alive connection: the connection
/// keeps working with the old certificate and new connections get the new
/// one.
#[tokio::test]
async fn reload_keeps_live_connections() {
    let server = Server::start(Acceptor::new(
        Store::new(load(&tls_config(vec![cert("exact", &[])]))),
        None,
    ))
    .await;
    let opts = ClientOpts::default();
    let mut live = Client::connect(server.addr, "api.example.com", &opts).await;
    assert_eq!(live.cert, "exact");
    assert_eq!(live.get("/").await.unwrap().0, 200);

    server
        .acceptor
        .store()
        .swap(load(&tls_config(vec![cert("rotated", &[])])));
    for _ in 0..3 {
        assert_eq!(
            live.get("/").await.unwrap(),
            (200, "api.example.com".to_owned())
        );
        assert!(live.is_open());
    }
    let fresh = Client::connect(server.addr, "api.example.com", &opts).await;
    assert_eq!(fresh.cert, "rotated");
    server.shutdown().await;
}

/// A handshake already running when the store is swapped finishes on the
/// snapshot it started with.
#[tokio::test]
async fn handshake_in_progress_keeps_its_snapshot() {
    let server = Server::start(Acceptor::new(
        Store::new(load(&tls_config(vec![cert("exact", &[])]))),
        None,
    ))
    .await;
    // Open the TCP connection and let the server start its handshake
    // before the swap, then send the ClientHello after it.
    let tcp = TcpStream::connect(server.addr).await.unwrap();
    wait_for(|| server.acceptor.in_progress() == 1).await;
    server
        .acceptor
        .store()
        .swap(load(&tls_config(vec![cert("rotated", &[])])));
    let name = rustls::pki_types::ServerName::try_from("api.example.com").unwrap();
    let s = tokio_rustls::TlsConnector::from(client_config(&ClientOpts::default()))
        .connect(name, tcp)
        .await
        .unwrap();
    assert_eq!(presented(&s), "exact");
    let s = handshake(server.addr, "api.example.com", &ClientOpts::default())
        .await
        .unwrap();
    assert_eq!(presented(&s), "rotated");
    server.shutdown().await;
}

/// The store is reloaded 100 times, each time from the files, while eight
/// clients open new connections and four keep-alive clients send requests
/// on theirs. No handshake and no request may fail, every new connection
/// gets one of the two certificates, and once the swaps stop a new
/// connection gets the last one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reload_100_times_under_load() {
    let a = tls_config(vec![cert("exact", &[]), cert("wild", &[])]);
    let b = tls_config(vec![cert("rotated", &[]), cert("wild", &[])]);
    let (on, seen) = counting();
    let server = Server::start(Acceptor::new(Store::new(load(&a)), Some(on))).await;
    let addr = server.addr;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let failures = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let certs = Arc::new(Mutex::new(std::collections::HashSet::new()));

    let mut clients = Vec::new();
    for _ in 0..8 {
        let (stop, failures, requests, certs) = (
            Arc::clone(&stop),
            Arc::clone(&failures),
            Arc::clone(&requests),
            Arc::clone(&certs),
        );
        clients.push(tokio::spawn(async move {
            let opts = ClientOpts::default();
            while !stop.load(Ordering::Relaxed) {
                match handshake(addr, "api.example.com", &opts).await {
                    Ok(s) => {
                        certs.lock().unwrap().insert(presented(&s));
                        let (mut send, conn) =
                            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(s))
                                .await
                                .unwrap();
                        tokio::spawn(conn);
                        let req = http::Request::get("/")
                            .body(http_body_util::Empty::<Bytes>::new())
                            .unwrap();
                        match send.send_request(req).await {
                            Ok(r) if r.status() == 200 => {
                                requests.fetch_add(1, Ordering::Relaxed);
                            }
                            other => {
                                eprintln!("request failed: {other:?}");
                                failures.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("handshake failed: {e}");
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    for _ in 0..4 {
        let (stop, failures, requests) = (
            Arc::clone(&stop),
            Arc::clone(&failures),
            Arc::clone(&requests),
        );
        clients.push(tokio::spawn(async move {
            let mut c = Client::connect(addr, "www.example.com", &ClientOpts::default()).await;
            while !stop.load(Ordering::Relaxed) {
                match c.get("/").await {
                    Ok((200, _)) => {
                        requests.fetch_add(1, Ordering::Relaxed);
                    }
                    other => {
                        eprintln!("keep-alive request failed: {other:?}");
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }

    for i in 0..100 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        let next = if i % 2 == 0 { &b } else { &a };
        server.acceptor.store().swap(load(next));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    stop.store(true, Ordering::Relaxed);
    for c in clients {
        c.await.unwrap();
    }
    assert_eq!(failures.load(Ordering::Relaxed), 0);
    let done = requests.load(Ordering::Relaxed);
    assert!(
        done >= 200,
        "only {done} requests completed during the reloads"
    );
    let certs = certs.lock().unwrap().clone();
    assert_eq!(
        certs,
        ["exact", "rotated"].into_iter().collect(),
        "new connections saw {certs:?}"
    );
    let handshakes = seen.lock().unwrap().clone();
    assert!(handshakes.iter().all(|r| *r == HandshakeResult::Ok));
    eprintln!(
        "100 reloads: {done} requests and {} handshakes, none failed",
        handshakes.len()
    );
    // The 100th swap installed `a`.
    let s = handshake(addr, "api.example.com", &ClientOpts::default())
        .await
        .unwrap();
    assert_eq!(presented(&s), "exact");
    server.shutdown().await;
}
