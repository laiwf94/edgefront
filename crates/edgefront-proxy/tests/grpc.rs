//! gRPC through the proxy against a tonic echo service: the four call
//! types with their messages, statuses and trailers intact, the proxy's
//! own errors as gRPC statuses, and `grpc-timeout` as the call's deadline.

#[path = "common/grpc_echo.rs"]
mod echo;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use edgefront_core::config::{Config, Http2};
use edgefront_core::h2::Settings;
use edgefront_core::listener::{ServerLimits, serve};
use edgefront_proxy::server::Proxy;
use tokio::net::TcpListener;
use tonic::Code;

fn config(up: SocketAddr, route_host: &str, timeout: &str) -> Config {
    let js = serde_json::json!({
        "listen": "127.0.0.1:0",
        "routes": [{"host": route_host, "cluster": "grpc"}],
        "clusters": [{
            "name": "grpc",
            "endpoints": [{"address": up.to_string()}],
            "protocol": "http2",
            "timeout": timeout,
        }],
    });
    Config::from_json(js.to_string().as_bytes()).unwrap()
}

/// The proxy for `config`, taking HTTP/2 with prior knowledge.
async fn proxy(config: &Config) -> SocketAddr {
    let p = Arc::new(Proxy::new(config));
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let limits = ServerLimits {
        http2: Some(Settings::from(&Http2 {
            cleartext: true,
            ..Http2::default()
        })),
        ..ServerLimits::default()
    };
    tokio::spawn(serve(socket, p, limits, std::future::pending()));
    addr
}

async fn through_proxy() -> tonic::client::Grpc<tonic::transport::Channel> {
    let up = echo::start().await;
    let addr = proxy(&config(up, "", "15s")).await;
    echo::client(addr).await
}

#[tokio::test]
async fn unary_calls_pass_through() {
    let mut grpc = through_proxy().await;
    let resp = echo::unary(&mut grpc, "hello", None).await.unwrap();
    assert_eq!(&resp.get_ref()[..], b"hello");
    assert_eq!(resp.metadata().get("seen-timeout").unwrap(), "none");
    // Large messages stream through flow control both ways.
    let big = "x".repeat(3 << 20);
    let resp = echo::unary(&mut grpc, &big, None).await.unwrap();
    assert_eq!(resp.get_ref().len(), 3 << 20);
}

#[tokio::test]
async fn server_streaming_calls_pass_through() {
    let mut grpc = through_proxy().await;
    let got = echo::server_stream(&mut grpc, 100).await.unwrap();
    assert_eq!(got.len(), 100);
    assert_eq!(got[0], "0");
    assert_eq!(got[99], "99");
    assert_eq!(
        echo::server_stream(&mut grpc, 0).await.unwrap(),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn client_streaming_calls_pass_through() {
    let mut grpc = through_proxy().await;
    let got = echo::client_stream(&mut grpc, &["a", "b", "c", "d"])
        .await
        .unwrap();
    assert_eq!(&got[..], b"a,b,c,d");
}

#[tokio::test]
async fn bidirectional_calls_interleave_through_the_proxy() {
    let mut grpc = through_proxy().await;
    // Each answer is read before the next message is sent, so nothing
    // along the way may hold a message back for a later one.
    let got = echo::bidi(&mut grpc, &["one", "two", "three"])
        .await
        .unwrap();
    assert_eq!(got, ["ONE", "TWO", "THREE"]);
}

#[tokio::test]
async fn statuses_and_trailers_from_the_service_are_kept() {
    let mut grpc = through_proxy().await;
    let err = echo::unary(&mut grpc, "fail", None).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    assert_eq!(err.message(), "nothing here");
    assert_eq!(err.metadata().get("x-detail").unwrap(), "why");
    // An unknown method is the service's own UNIMPLEMENTED.
    grpc.ready().await.unwrap();
    let err = grpc
        .unary(
            tonic::Request::new(bytes::Bytes::new()),
            echo::path("Nope"),
            echo::Raw,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unimplemented);
    assert_eq!(err.message(), "no such method");
}

#[tokio::test]
async fn the_proxys_errors_are_grpc_statuses() {
    let up = echo::start().await;
    // No route for the client's authority.
    let addr = proxy(&config(up, "only.example.com", "15s")).await;
    let mut grpc = echo::client(addr).await;
    let err = echo::unary(&mut grpc, "hi", None).await.unwrap_err();
    assert_eq!(err.code(), Code::Unimplemented);
    assert_eq!(err.message(), "no route matches this host and path");
    assert!(err.metadata().get("x-request-id").is_some());

    // An endpoint that is not there.
    let gone = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = gone.local_addr().unwrap();
    drop(gone);
    let addr = proxy(&config(dead, "", "15s")).await;
    let mut grpc = echo::client(addr).await;
    let err = echo::unary(&mut grpc, "hi", None).await.unwrap_err();
    assert_eq!(err.code(), Code::Unavailable);
    assert_eq!(err.message(), "the upstream failed to answer");

    // The cluster's own timeout.
    let addr = proxy(&config(up, "", "100ms")).await;
    let mut grpc = echo::client(addr).await;
    let err = echo::unary(&mut grpc, "sleep:2000", None)
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::DeadlineExceeded);
}

#[tokio::test]
async fn grpc_timeout_is_the_calls_deadline() {
    let mut grpc = through_proxy().await;
    // The service is told the time left, never more than asked for.
    let resp = echo::unary(&mut grpc, "quick", Some(Duration::from_secs(5)))
        .await
        .unwrap();
    let seen = resp
        .metadata()
        .get("seen-timeout")
        .unwrap()
        .to_str()
        .unwrap();
    let ms: u64 = seen.strip_suffix('m').unwrap().parse().unwrap();
    assert!((4_900..=5_000).contains(&ms), "{seen}");
    // A call slower than its deadline ends at the deadline, inside the
    // cluster's 15 s, with the proxy's DEADLINE_EXCEEDED. tonic's channel
    // would time the call out itself, so this one is sent by hand.
    let up = echo::start().await;
    let addr = proxy(&config(up, "", "15s")).await;
    let start = Instant::now();
    let (status, headers) = raw_unary(addr, b"sleep:3000", "200m").await;
    let took = start.elapsed();
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "application/grpc");
    assert_eq!(headers["grpc-status"], "4", "DEADLINE_EXCEEDED");
    assert_eq!(
        headers["grpc-message"],
        "the upstream did not answer in time"
    );
    assert!(took >= Duration::from_millis(190), "{took:?}");
    assert!(took < Duration::from_millis(1500), "{took:?}");
}

/// One unary gRPC call over a bare HTTP/2 connection, with `grpc-timeout`
/// set to `timeout`: the response status and its headers (a Trailers-Only
/// answer carries `grpc-status` there).
async fn raw_unary(addr: SocketAddr, message: &[u8], timeout: &str) -> (u16, http::HeaderMap) {
    use http_body_util::{BodyExt, Full};
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut send, conn) = hyper::client::conn::http2::handshake(
        hyper_util::rt::TokioExecutor::new(),
        hyper_util::rt::TokioIo::new(tcp),
    )
    .await
    .unwrap();
    tokio::spawn(conn);
    let mut frame = vec![0u8];
    frame.extend_from_slice(&(message.len() as u32).to_be_bytes());
    frame.extend_from_slice(message);
    let req = http::Request::post(format!("http://{addr}/echo.Echo/Unary"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .header("grpc-timeout", timeout)
        .body(Full::new(bytes::Bytes::from(frame)))
        .unwrap();
    let resp = send.send_request(req).await.unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let _ = resp.into_body().collect().await;
    (status, headers)
}
