//! Per-connection quotas ([`ConnQuota`], given by a handler): the request
//! that reaches a connection's quota is the last on it. Over HTTP/1.1 its
//! response says `Connection: close` and the connection closes after it;
//! over HTTP/2 the connection is sent GOAWAY, streams already open finish,
//! and the client's connection ends.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use edgefront_core::config::Http2;
use edgefront_core::h2::Settings;
use edgefront_core::listener::{ConnQuota, Handler, ServerLimits, serve};
use http::{Request, Response};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct Quota(ConnQuota);

impl Handler for Quota {
    type Body = Full<Bytes>;

    async fn call(
        self: Arc<Self>,
        _req: Request<Incoming>,
        _: SocketAddr,
    ) -> Response<Full<Bytes>> {
        Response::new(Full::new(Bytes::from_static(b"ok")))
    }

    fn conn_quota(&self) -> ConnQuota {
        self.0
    }
}

async fn start(quota: ConnQuota) -> SocketAddr {
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
        Arc::new(Quota(quota)),
        limits,
        std::future::pending(),
    ));
    addr
}

#[tokio::test]
async fn http1_closes_after_the_quotas_last_request() {
    let addr = start(ConnQuota {
        max_requests: Some(2),
        max_bytes: None,
    })
    .await;
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(b"GET /1 HTTP/1.1\r\nhost: t\r\n\r\nGET /2 HTTP/1.1\r\nhost: t\r\n\r\nGET /3 HTTP/1.1\r\nhost: t\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    tokio::time::timeout(Duration::from_secs(5), c.read_to_string(&mut out))
        .await
        .expect("the connection closes")
        .unwrap();
    assert_eq!(out.matches("HTTP/1.1 200").count(), 2, "{out}");
    let second = out.rsplit("HTTP/1.1 200").next().unwrap();
    assert!(
        second.to_ascii_lowercase().contains("connection: close"),
        "{out}"
    );
    // A byte quota: the request that starts past it is the last.
    let addr = start(ConnQuota {
        max_requests: None,
        max_bytes: Some(1),
    })
    .await;
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(b"GET /1 HTTP/1.1\r\nhost: t\r\n\r\nGET /2 HTTP/1.1\r\nhost: t\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    c.read_to_string(&mut out).await.unwrap();
    assert_eq!(out.matches("HTTP/1.1 200").count(), 1, "{out}");
}

#[tokio::test]
async fn http2_gets_goaway_after_the_quotas_last_stream() {
    let addr = start(ConnQuota {
        max_requests: Some(3),
        max_bytes: None,
    })
    .await;
    let io = TokioIo::new(TcpStream::connect(addr).await.unwrap());
    let (mut sender, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io)
        .await
        .unwrap();
    let driver = tokio::spawn(conn);
    for i in 0..3 {
        sender.ready().await.unwrap();
        let resp = sender
            .send_request(
                Request::get(format!("http://t/{i}"))
                    .body(Full::new(Bytes::new()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.into_body().collect().await.unwrap().to_bytes(), "ok");
    }
    // After the third, the server's GOAWAY ends the connection.
    let ended = tokio::time::timeout(Duration::from_secs(5), driver).await;
    assert!(ended.is_ok(), "the connection ended after GOAWAY");
    // A fourth stream on it is refused.
    let r = sender
        .send_request(
            Request::get("http://t/4")
                .body(Full::new(Bytes::new()))
                .unwrap(),
        )
        .await;
    assert!(r.is_err());
    // No quota: many requests on one connection.
    let addr = start(ConnQuota::default()).await;
    let mut c = TcpStream::connect(addr).await.unwrap();
    for _ in 0..20 {
        c.write_all(b"GET / HTTP/1.1\r\nhost: t\r\n\r\n")
            .await
            .unwrap();
    }
    c.write_all(b"GET / HTTP/1.1\r\nhost: t\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    c.read_to_string(&mut out).await.unwrap();
    assert_eq!(out.matches("HTTP/1.1 200").count(), 21);
}
