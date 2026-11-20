//! The HTTP/2 listener: ALPN on TLS, prior knowledge on cleartext, the
//! settings it advertises, HTTP/1.1 beside it, and h2spec's whole server
//! suite against it in Docker.

#[path = "common/tls.rs"]
mod common;

use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::*;
use edgefront_core::config::Http2;
use edgefront_core::h2::{PREFACE, Settings};
use edgefront_core::listener::{Handler, ServerLimits, serve, serve_tls};
use edgefront_core::tls::{Acceptor, Shared, Snapshot, Store};
use http::{Request, Response, Version};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// h2spec 2.6.0, pinned by digest.
const H2SPEC: &str =
    "summerwind/h2spec@sha256:5f4a65c30cae8569558ced048b4bfe0dcf01a221e36767ae504ccd8348a7aeb0";
const ALPINE: &str = "alpine:3.20";

/// Answers every request with its protocol version and the size of the
/// body it read, after a fixed 1 KiB of text so the response has DATA
/// frames to flow-control.
struct Echo;

impl Handler for Echo {
    type Body = Full<Bytes>;

    async fn call(self: Arc<Self>, req: Request<Incoming>, _: SocketAddr) -> Response<Full<Bytes>> {
        let version = format!("{:?}", req.version());
        let read = match req.into_body().collect().await {
            Ok(b) => b.to_bytes().len(),
            Err(_) => 0,
        };
        let mut text = format!("{version} read {read}\n");
        text.push_str(&"x".repeat(1024));
        Response::new(Full::new(Bytes::from(text)))
    }
}

fn settings(cleartext: bool) -> Settings {
    Settings::from(&Http2 {
        cleartext,
        ..Http2::default()
    })
}

fn limits(http2: Option<Settings>) -> ServerLimits {
    ServerLimits {
        drain_timeout: Duration::from_secs(2),
        http2,
        ..ServerLimits::default()
    }
}

struct Server {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<edgefront_core::listener::ServeReport>,
}

impl Server {
    async fn plain(bind: &str, limits: ServerLimits) -> Server {
        let socket = TcpListener::bind(bind).await.unwrap();
        let addr = socket.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(serve(socket, Arc::new(Echo), limits, async move {
            let _ = stopped.await;
        }));
        Server {
            addr,
            stop: Some(stop),
            task,
        }
    }

    async fn tls(bind: &str, limits: ServerLimits, http2: bool) -> Server {
        let shared = Shared {
            http2,
            ..Shared::default()
        };
        let snapshot = Snapshot::load_with(&tls_config(vec![cert("default", &[])]), &shared)
            .expect("the fixture loads");
        let acceptor = Acceptor::new(Store::new(Arc::new(snapshot)), None);
        let socket = TcpListener::bind(bind).await.unwrap();
        let addr = socket.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(serve_tls(
            socket,
            acceptor,
            Arc::new(Echo),
            limits,
            async move {
                let _ = stopped.await;
            },
        ));
        Server {
            addr,
            stop: Some(stop),
            task,
        }
    }

    async fn shutdown(mut self) -> edgefront_core::listener::ServeReport {
        let _ = self.stop.take().unwrap().send(());
        self.task.await.unwrap()
    }
}

/// Sends `n` GETs over one HTTP/2 connection on `io`, all at once, and
/// returns each response's version and body.
async fn h2_requests<I>(io: I, n: usize) -> Vec<(Version, String)>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(io))
            .await
            .expect("an HTTP/2 handshake");
    tokio::spawn(conn);
    let mut tasks = Vec::new();
    for i in 0..n {
        let mut sender = sender.clone();
        tasks.push(tokio::spawn(async move {
            let req = Request::post(format!("http://edge.test/{i}"))
                .body(Full::new(Bytes::from(vec![b'a'; i])))
                .unwrap();
            let resp = sender.send_request(req).await.expect("a response");
            let version = resp.version();
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            (version, String::from_utf8_lossy(&body).into_owned())
        }));
    }
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.unwrap());
    }
    out
}

#[tokio::test]
async fn tls_negotiates_h2_and_serves_streams_concurrently() {
    let server = Server::tls("127.0.0.1:0", limits(Some(settings(false))), true).await;
    let tls = handshake(
        server.addr,
        "",
        &ClientOpts {
            roots: None,
            alpn: vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            ..ClientOpts::default()
        },
    )
    .await
    .expect("a handshake");
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    let answers = h2_requests(tls, 50).await;
    for (i, (version, body)) in answers.iter().enumerate() {
        assert_eq!(*version, Version::HTTP_2);
        assert!(body.starts_with(&format!("HTTP/2.0 read {i}\n")), "{body}");
    }
    // An HTTP/1.1 client on the same listener still gets HTTP/1.1.
    let mut client = Client::connect(
        server.addr,
        "",
        &ClientOpts {
            roots: None,
            ..ClientOpts::default()
        },
    )
    .await;
    let (status, body) = client.get("/").await.unwrap();
    assert_eq!(status, 200);
    assert!(body.starts_with("HTTP/1.1 read 0"));
    drop(client);
    assert_eq!(server.shutdown().await.forced, 0);
}

#[tokio::test]
async fn without_http2_tls_offers_only_http11() {
    let server = Server::tls("127.0.0.1:0", limits(None), false).await;
    let tls = handshake(
        server.addr,
        "",
        &ClientOpts {
            roots: None,
            alpn: vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            ..ClientOpts::default()
        },
    )
    .await
    .expect("a handshake");
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
    // A client offering only h2 is refused, as before.
    let refused = handshake(
        server.addr,
        "",
        &ClientOpts {
            roots: None,
            alpn: vec![b"h2".to_vec()],
            ..ClientOpts::default()
        },
    )
    .await;
    assert!(refused.is_err());
    server.shutdown().await;
}

#[tokio::test]
async fn cleartext_prior_knowledge_and_http11_share_a_listener() {
    let server = Server::plain("127.0.0.1:0", limits(Some(settings(true)))).await;
    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let answers = h2_requests(tcp, 10).await;
    assert!(answers.iter().all(|(v, _)| *v == Version::HTTP_2));
    // HTTP/1.1 requests whose first bytes overlap the preface ("P").
    for method in ["POST", "PUT", "GET"] {
        let mut tcp = TcpStream::connect(server.addr).await.unwrap();
        tcp.write_all(
            format!("{method} / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc").as_bytes(),
        )
        .await
        .unwrap();
        let mut out = String::new();
        tcp.read_to_string(&mut out).await.unwrap();
        assert!(out.starts_with("HTTP/1.1 200 OK"), "{out}");
        assert!(out.contains("HTTP/1.1 read 3"), "{out}");
    }
    server.shutdown().await;
}

#[tokio::test]
async fn cleartext_http2_is_off_unless_configured() {
    // HTTP/2 settings without `cleartext`: a plain listener stays HTTP/1.1,
    // so the preface is a bad request.
    let server = Server::plain("127.0.0.1:0", limits(Some(settings(false)))).await;
    let mut tcp = TcpStream::connect(server.addr).await.unwrap();
    tcp.write_all(PREFACE).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), tcp.read_to_end(&mut out)).await;
    let out = String::from_utf8_lossy(&out);
    assert!(!out.contains("HTTP/2"), "{out}");
    server.shutdown().await;
}

/// Reads one frame: its type, flags, stream and payload.
async fn frame(tcp: &mut TcpStream) -> (u8, u8, u32, Vec<u8>) {
    let mut head = [0u8; 9];
    tcp.read_exact(&mut head).await.unwrap();
    let len = u32::from_be_bytes([0, head[0], head[1], head[2]]) as usize;
    let mut payload = vec![0u8; len];
    tcp.read_exact(&mut payload).await.unwrap();
    let stream = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff;
    (head[3], head[4], stream, payload)
}

#[tokio::test]
async fn the_configured_settings_are_advertised() {
    let configured = Settings::from(&Http2 {
        max_concurrent_streams: 7,
        initial_stream_window: 100_000,
        initial_connection_window: 3_000_000,
        max_frame_size: 32_768,
        max_header_list_size: 8_192,
        cleartext: true,
        adaptive_window: false,
    });
    let server = Server::plain("127.0.0.1:0", limits(Some(configured))).await;
    let mut tcp = TcpStream::connect(server.addr).await.unwrap();
    tcp.write_all(PREFACE).await.unwrap();
    // An empty SETTINGS frame.
    tcp.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]).await.unwrap();
    let (kind, flags, stream, payload) = frame(&mut tcp).await;
    assert_eq!(
        (kind, flags, stream),
        (4, 0, 0),
        "the server's SETTINGS first"
    );
    let mut seen = std::collections::HashMap::new();
    for s in payload.chunks(6) {
        let id = u16::from_be_bytes([s[0], s[1]]);
        let v = u32::from_be_bytes([s[2], s[3], s[4], s[5]]);
        seen.insert(id, v);
    }
    assert_eq!(seen.get(&0x3), Some(&7), "MAX_CONCURRENT_STREAMS");
    assert_eq!(seen.get(&0x4), Some(&100_000), "INITIAL_WINDOW_SIZE");
    assert_eq!(seen.get(&0x5), Some(&32_768), "MAX_FRAME_SIZE");
    assert_eq!(seen.get(&0x6), Some(&8_192), "MAX_HEADER_LIST_SIZE");
    // Then the connection window is raised by WINDOW_UPDATE on stream 0.
    let mut increment = None;
    for _ in 0..4 {
        let (kind, _, stream, payload) = frame(&mut tcp).await;
        if kind == 8 && stream == 0 {
            increment = Some(u32::from_be_bytes(payload[..4].try_into().unwrap()));
            break;
        }
    }
    assert_eq!(increment, Some(3_000_000 - 65_535));
    drop(tcp);
    server.shutdown().await;
}

#[tokio::test]
async fn shutdown_sends_goaway_and_finishes_open_streams() {
    let server = Server::plain("127.0.0.1:0", limits(Some(settings(true)))).await;
    let tcp = TcpStream::connect(server.addr).await.unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
            .await
            .unwrap();
    let conn = tokio::spawn(conn);
    let resp = sender
        .send_request(
            Request::get("http://edge.test/")
                .body(Full::new(Bytes::new()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let report = server.shutdown().await;
    assert_eq!(report.forced, 0);
    // The client's connection ends cleanly after the server's GOAWAY.
    let ended = tokio::time::timeout(Duration::from_secs(5), conn).await;
    assert!(ended.is_ok(), "the connection closed");
}

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("docker {args:?}: {e}; the h2spec test needs Docker"));
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

/// The host's address as containers on the default network reach it.
fn host_ip() -> String {
    docker(&[
        "run",
        "--rm",
        "--add-host=host.docker.internal:host-gateway",
        ALPINE,
        "getent",
        "hosts",
        "host.docker.internal",
    ])
    .split_whitespace()
    .next()
    .expect("host.docker.internal resolves")
    .to_owned()
}

/// Runs h2spec's every case against a listener and returns its summary
/// line and the full report.
fn h2spec(port: u16, tls: bool) -> (String, String) {
    let host = host_ip();
    let port = port.to_string();
    let mut args = vec![
        "run",
        "--rm",
        "--add-host=host.docker.internal:host-gateway",
        "--platform=linux/amd64",
        H2SPEC,
        "-h",
        &host,
        "-p",
        &port,
        "-o",
        "5",
    ];
    if tls {
        args.extend(["-t", "-k"]);
    }
    let report = docker(&args);
    let summary = report
        .lines()
        .rev()
        .find(|l| l.contains("tests,"))
        .unwrap_or_default()
        .trim()
        .to_owned();
    (summary, report)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h2spec_passes_every_server_case() {
    let tls = Server::tls("0.0.0.0:0", limits(Some(settings(false))), true).await;
    let plain = Server::plain("0.0.0.0:0", limits(Some(settings(true)))).await;
    let (tls_port, plain_port) = (tls.addr.port(), plain.addr.port());
    let ((tls_summary, tls_report), (plain_summary, plain_report)) =
        tokio::task::spawn_blocking(move || (h2spec(tls_port, true), h2spec(plain_port, false)))
            .await
            .unwrap();
    assert_eq!(
        tls_summary, "146 tests, 146 passed, 0 skipped, 0 failed",
        "h2spec over TLS:\n{tls_report}"
    );
    // A cleartext listener also serves HTTP/1.1 on the same port, so a
    // client whose first bytes are not the preface is an HTTP/1.1 client:
    // h2spec's "invalid connection preface" case gets an HTTP/1.1 400
    // there, and only that case may fail.
    assert_eq!(
        plain_summary, "146 tests, 145 passed, 0 skipped, 1 failed",
        "h2spec over cleartext:\n{plain_report}"
    );
    let failures = &plain_report[plain_report.find("Failures:").expect("a failure list")..];
    assert_eq!(failures.matches('×').count(), 1, "{failures}");
    assert!(
        failures.contains("2: Sends invalid connection preface"),
        "{failures}"
    );
    tls.shutdown().await;
    plain.shutdown().await;
}
