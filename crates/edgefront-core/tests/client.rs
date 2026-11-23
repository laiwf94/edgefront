//! The pooled client's limits against hand-written origins.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use edgefront_core::http1::{Client, ClientError, ClientLimits};
use http::Request;
use http_body_util::{BodyExt, Empty};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};

/// How a fake origin answers each request.
#[derive(Clone, Copy)]
enum Reply {
    /// 200 with a short body.
    Ok,
    /// Reads the request and never answers.
    Silent,
    /// 200 whose head is exactly this many bytes.
    HeadOfLen(usize),
    /// Something that is not HTTP.
    Garbage,
}

struct Origin {
    addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
}

impl Origin {
    async fn start(reply: Reply) -> Origin {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let count = accepted.clone();
        tokio::spawn(async move {
            loop {
                let (s, _) = listener.accept().await.unwrap();
                count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(answer(s, reply));
            }
        });
        Origin { addr, accepted }
    }

    fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

async fn answer(mut s: TcpStream, reply: Reply) {
    loop {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match s.read(&mut byte).await {
                Ok(1) => head.push(byte[0]),
                _ => return,
            }
        }
        let out = match reply {
            Reply::Ok => b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok".to_vec(),
            Reply::Silent => {
                std::future::pending::<()>().await;
                unreachable!()
            }
            Reply::HeadOfLen(len) => {
                let bare = "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nx-pad: \r\n\r\n".len();
                let pad = "a".repeat(len - bare);
                let head = format!("HTTP/1.1 200 OK\r\ncontent-length: 0\r\nx-pad: {pad}\r\n\r\n");
                assert_eq!(head.len(), len);
                head.into_bytes()
            }
            Reply::Garbage => b"HELLO THERE\r\n\r\n".to_vec(),
        };
        if s.write_all(&out).await.is_err() {
            return;
        }
    }
}

fn limits() -> ClientLimits {
    ClientLimits {
        connect_timeout: Duration::from_millis(300),
        response_header_timeout: Some(Duration::from_millis(500)),
        pool_idle_timeout: Duration::from_secs(30),
        max_idle_per_host: 8,
        max_response_header_bytes: 16 << 10,
    }
}

fn get(url: &str) -> Request<Empty<Bytes>> {
    Request::get(url).body(Empty::new()).unwrap()
}

/// Sends a GET and reads the whole body, so the connection can be pooled.
async fn fetch(client: &Client<Empty<Bytes>>, url: &str) -> Result<Bytes, ClientError> {
    let resp = client.send(get(url)).await?;
    assert_eq!(resp.status(), 200);
    Ok(resp.into_body().collect().await.unwrap().to_bytes())
}

#[tokio::test]
async fn sequential_requests_reuse_one_pooled_connection() {
    let origin = Origin::start(Reply::Ok).await;
    let client = Client::new(&limits());
    for _ in 0..5 {
        assert_eq!(fetch(&client, &origin.url()).await.unwrap(), "ok");
    }
    assert_eq!(origin.accepted(), 1);
    // Clones share the pool.
    let clone = client.clone();
    fetch(&clone, &origin.url()).await.unwrap();
    assert_eq!(origin.accepted(), 1);
    assert!(format!("{clone:?}").contains("response_header_timeout"));
}

#[tokio::test]
async fn zero_idle_connections_per_host_turns_pooling_off() {
    let origin = Origin::start(Reply::Ok).await;
    let client = Client::new(&ClientLimits {
        max_idle_per_host: 0,
        ..limits()
    });
    for _ in 0..3 {
        fetch(&client, &origin.url()).await.unwrap();
    }
    assert_eq!(origin.accepted(), 3);
}

#[tokio::test]
async fn idle_connections_per_host_are_capped() {
    let origin = Origin::start(Reply::Ok).await;
    let client = Client::new(&ClientLimits {
        max_idle_per_host: 1,
        ..limits()
    });
    let url = origin.url();
    // Three requests at once need three connections; only one is kept.
    let (a, b, c) = tokio::join!(
        fetch(&client, &url),
        fetch(&client, &url),
        fetch(&client, &url)
    );
    a.unwrap();
    b.unwrap();
    c.unwrap();
    assert_eq!(origin.accepted(), 3);
    let (a, b) = tokio::join!(fetch(&client, &url), fetch(&client, &url));
    a.unwrap();
    b.unwrap();
    assert_eq!(origin.accepted(), 4);
}

#[tokio::test]
async fn idle_pooled_connections_expire() {
    let origin = Origin::start(Reply::Ok).await;
    let client = Client::new(&ClientLimits {
        pool_idle_timeout: Duration::from_millis(200),
        ..limits()
    });
    fetch(&client, &origin.url()).await.unwrap();
    fetch(&client, &origin.url()).await.unwrap();
    assert_eq!(origin.accepted(), 1);
    tokio::time::sleep(Duration::from_millis(600)).await;
    fetch(&client, &origin.url()).await.unwrap();
    assert_eq!(origin.accepted(), 2);
}

#[tokio::test]
async fn a_silent_origin_times_out_waiting_for_the_head() {
    let origin = Origin::start(Reply::Silent).await;
    let client = Client::new(&limits());
    let start = Instant::now();
    assert_eq!(
        fetch(&client, &origin.url()).await,
        Err(ClientError::Timeout)
    );
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(480), "{took:?}");
    assert!(took < Duration::from_millis(1500), "{took:?}");
}

#[tokio::test]
async fn without_a_head_timeout_the_client_waits() {
    let origin = Origin::start(Reply::Silent).await;
    let client = Client::new(&ClientLimits {
        response_header_timeout: None,
        ..limits()
    });
    let url = origin.url();
    let waited = tokio::time::timeout(Duration::from_millis(800), fetch(&client, &url));
    assert!(waited.await.is_err());
}

#[tokio::test]
async fn response_heads_over_the_limit_are_refused() {
    let limit = limits().max_response_header_bytes;
    let fits = Origin::start(Reply::HeadOfLen(limit)).await;
    let client = Client::new(&limits());
    assert_eq!(fetch(&client, &fits.url()).await.unwrap(), "");

    let over = Origin::start(Reply::HeadOfLen(limit + 1)).await;
    assert_eq!(
        fetch(&client, &over.url()).await,
        Err(ClientError::HeadersTooLarge)
    );
}

#[tokio::test]
async fn a_refused_connection_is_a_connect_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let client = Client::new(&limits());
    let url = format!("http://{addr}/");
    assert_eq!(fetch(&client, &url).await, Err(ClientError::Connect));
}

#[tokio::test]
async fn a_hanging_connect_hits_the_connect_timeout() {
    // A listener that never accepts, with its accept queue already full:
    // the kernel drops further SYNs, so a connect hangs.
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(1).unwrap();
    let addr = listener.local_addr().unwrap();
    let mut fillers = Vec::new();
    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_millis(100), TcpStream::connect(addr)).await {
            Ok(Ok(s)) => fillers.push(s),
            _ => break,
        }
    }
    let client = Client::new(&limits());
    let start = Instant::now();
    let url = format!("http://{addr}/");
    assert_eq!(fetch(&client, &url).await, Err(ClientError::ConnectTimeout));
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(280), "{took:?}");
    assert!(took < Duration::from_millis(1500), "{took:?}");
    drop(listener);
}

#[tokio::test]
async fn a_non_http_reply_is_a_protocol_error() {
    let origin = Origin::start(Reply::Garbage).await;
    let client = Client::new(&limits());
    assert_eq!(
        fetch(&client, &origin.url()).await,
        Err(ClientError::Protocol)
    );
}

/// An origin over TLS (the `exact` fixture, for api.example.com) that
/// answers each request with its own head, lowercased.
async fn echoing_tls_origin() -> SocketAddr {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tls");
    let certs = CertificateDer::pem_file_iter(format!("{dir}/exact.crt"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_file(format!("{dir}/exact.key")).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.ends_with(b"\r\n\r\n") {
                    match tls.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let body = String::from_utf8_lossy(&head).to_lowercase();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tls.write_all(reply.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    addr
}

/// Over TLS the request line is the origin form, so the Host the plain
/// pool takes from the URI must be written out: an origin such as the
/// Kubernetes API server (Go's net/http) refuses an HTTP/1.1 request
/// without one. A Host the caller set is kept, and sent once.
#[tokio::test]
async fn tls_requests_carry_the_host_of_their_uri() {
    let addr = echoing_tls_origin().await;
    let tls = edgefront_core::http1::TlsTo::load(&edgefront_core::config::UpstreamTls {
        ca_file: concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tls/ca.crt").to_owned(),
        server_name: Some("api.example.com".to_owned()),
        cert_file: None,
        key_file: None,
    })
    .unwrap();
    let client = Client::with_tls(&limits(), tls);

    let head = fetch(&client, &format!("http://{addr}/list?x=1"))
        .await
        .unwrap();
    let head = String::from_utf8(head.to_vec()).unwrap();
    assert!(head.starts_with("get /list?x=1 http/1.1\r\n"), "{head}");
    assert_eq!(head.matches("\r\nhost: ").count(), 1, "{head}");
    assert!(head.contains(&format!("\r\nhost: {addr}\r\n")), "{head}");

    let req = Request::get(format!("http://{addr}/"))
        .header(http::header::HOST, "api.example.com")
        .body(Empty::new())
        .unwrap();
    let resp = client.send(req).await.unwrap();
    let head = resp.into_body().collect().await.unwrap().to_bytes();
    let head = String::from_utf8(head.to_vec()).unwrap();
    assert_eq!(head.matches("\r\nhost: ").count(), 1, "{head}");
    assert!(head.contains("\r\nhost: api.example.com\r\n"), "{head}");
}
