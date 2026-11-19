//! The load generator over TLS: a scenario with `[tls]` loads an HTTPS
//! server with the scenario's SNI and ALPN, every request answered, and
//! the control client's readiness check works over TLS too.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use edgefront_bench::client;
use edgefront_bench::load::{self, LoadConfig, Protocol, Schedule};
use edgefront_bench::scenario::Scenario;
use edgefront_bench::tls::TlsClient;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::TcpListener;

fn fixture(name: &str) -> String {
    format!(
        "{}/../edgefront-core/tests/fixtures/tls/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// What the server saw: handshakes, the last SNI and ALPN, and requests.
#[derive(Default)]
struct Seen {
    handshakes: AtomicUsize,
    requests: AtomicUsize,
    sni: std::sync::Mutex<Option<String>>,
    alpn: std::sync::Mutex<Option<Vec<u8>>>,
}

async fn https_server(seen: Arc<Seen>) -> SocketAddr {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(fixture("exact.crt"))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let key = PrivateKeyDer::from_pem_file(fixture("exact.key")).unwrap();
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = socket.accept().await {
            let (acceptor, seen) = (acceptor.clone(), Arc::clone(&seen));
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                seen.handshakes.fetch_add(1, Ordering::SeqCst);
                let conn = tls.get_ref().1;
                *seen.sni.lock().unwrap() = conn.server_name().map(str::to_owned);
                let alpn = conn.alpn_protocol().map(<[u8]>::to_vec);
                *seen.alpn.lock().unwrap() = alpn.clone();
                let svc = hyper::service::service_fn(move |_: http::Request<Incoming>| {
                    let seen = Arc::clone(&seen);
                    async move {
                        seen.requests.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, Infallible>(http::Response::new(Full::new(Bytes::from_static(
                            b"ok",
                        ))))
                    }
                });
                let io = TokioIo::new(tls);
                if alpn.as_deref() == Some(b"h2") {
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(io, svc)
                        .await;
                } else {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, svc)
                        .await;
                }
            });
        }
    });
    addr
}

fn get(path: &str) -> Arc<dyn load::RequestSource> {
    let path = path.to_owned();
    Arc::new(move |_seq: u64| {
        http::Request::get(path.as_str())
            .body(Full::new(Bytes::new()))
            .unwrap()
    })
}

#[tokio::test]
async fn loads_an_https_server_over_both_protocols() {
    for (protocol, alpn) in [
        (Protocol::Http1, &b"http/1.1"[..]),
        (Protocol::Http2, b"h2"),
    ] {
        let seen = Arc::new(Seen::default());
        let addr = https_server(Arc::clone(&seen)).await;
        let tls = TlsClient::new("api.example.com", protocol).unwrap();
        assert_eq!(tls.server_name(), "api.example.com");
        // The control client speaks HTTP/1.1 whatever the load speaks.
        let control = TlsClient::new("api.example.com", Protocol::Http1).unwrap();
        client::wait_ready_over(addr, "/", Duration::from_secs(5), Some(&control))
            .await
            .unwrap();
        let mut config = LoadConfig::new(
            addr,
            protocol,
            Schedule::constant(200.0, Duration::from_secs(1)),
        );
        config.connections = 4;
        config.tls = Some(tls);
        let report = load::run(config, get("/")).await.unwrap();
        let measured = report.measured();
        assert_eq!(measured.completed, 200, "{protocol:?}");
        assert_eq!(measured.errors.total(), 0, "{protocol:?}");
        assert_eq!(seen.sni.lock().unwrap().as_deref(), Some("api.example.com"));
        assert_eq!(seen.alpn.lock().unwrap().as_deref(), Some(alpn));
        // The readiness probe's connection plus one per load connection.
        assert_eq!(seen.handshakes.load(Ordering::SeqCst), 5, "{protocol:?}");
        assert_eq!(seen.requests.load(Ordering::SeqCst), 201);
    }
}

#[tokio::test]
async fn cleartext_against_tls_fails_as_connect_errors() {
    let addr = https_server(Arc::new(Seen::default())).await;
    let mut config = LoadConfig::new(
        addr,
        Protocol::Http1,
        Schedule::constant(20.0, Duration::from_millis(500)),
    );
    config.connections = 1;
    config.timeout = Duration::from_millis(500);
    let report = load::run(config, get("/")).await.unwrap();
    assert_eq!(report.measured().completed, 0);
    // And TLS against a cleartext server.
    let plain = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let plain_addr = plain.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = plain.accept().await {
            use tokio::io::AsyncWriteExt;
            let _ = tcp.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await;
        }
    });
    let tls = TlsClient::new("api.example.com", Protocol::Http1).unwrap();
    let err = client::get_over(plain_addr, "/", Duration::from_secs(2), Some(&tls))
        .await
        .unwrap_err();
    assert!(err.contains("tls handshake"), "{err}");
}

#[test]
fn scenarios_name_their_server() {
    let base = r#"name = "t"
protocol = "http1"
[load]
rate = 10
duration = "1s"
[keys]
count = 10
hit_ratio = 0.0
[[objects]]
size = "1KiB"
"#;
    assert_eq!(Scenario::from_toml(base).unwrap().tls, None);
    let s =
        Scenario::from_toml(&format!("{base}[tls]\nserver_name = \"api.example.com\"\n")).unwrap();
    assert_eq!(s.tls.as_deref(), Some("api.example.com"));
    let err = Scenario::from_toml(&format!("{base}[tls]\nserver_name = \"not a name\"\n"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("tls.server_name"), "{err}");
    assert!(Scenario::from_toml(&format!("{base}[tls]\nsni = \"x\"\n")).is_err());
    assert!(TlsClient::new("not a name", Protocol::Http1).is_err());
    // Every steer-port scenario reads, the TLS ones with their SNI.
    let dir = format!(
        "{}/../../bench/scenarios/steer-port",
        env!("CARGO_MANIFEST_DIR")
    );
    let all = Scenario::load_dir(&dir).unwrap();
    assert_eq!(all.len(), 8);
    for s in &all {
        assert_eq!(s.tls.is_some(), s.name.contains("-tls-"), "{}", s.name);
    }
}
