//! Every combination of a cluster's `protocol` (http1, http2, auto) with
//! an endpoint that speaks HTTP/1.1 only or HTTP/2 too, in cleartext and
//! over TLS: the version the endpoint saw, a 502 where the cluster asks
//! for a protocol the endpoint lacks, and the downgrade under `auto`,
//! which holds for later requests.

#[path = "../../edgefront-core/tests/common/tls.rs"]
mod tlsc;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use edgefront_core::config::{Config, Http2};
use edgefront_core::h2::Settings;
use edgefront_core::listener::{Handler, ServeReport, ServerLimits, serve, serve_tls};
use edgefront_core::tls::{Acceptor, Shared, Snapshot, Store};
use edgefront_proxy::server::Proxy;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Answers with the request's version, its `:authority` or `Host`, and its
/// body's length.
struct Version;

impl Handler for Version {
    type Body = Full<Bytes>;

    async fn call(
        self: Arc<Self>,
        req: http::Request<Incoming>,
        _: SocketAddr,
    ) -> http::Response<Full<Bytes>> {
        let version = format!("{:?}", req.version());
        let host = req
            .uri()
            .authority()
            .map(|a| a.to_string())
            .or_else(|| {
                req.headers()
                    .get(http::header::HOST)
                    .and_then(|h| h.to_str().ok())
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        let len = req
            .into_body()
            .collect()
            .await
            .map_or(0, |b| b.to_bytes().len());
        http::Response::new(Full::new(Bytes::from(format!("{version} {host} {len}"))))
    }
}

#[derive(Clone, Copy, Debug)]
enum Origin {
    Http1,
    Http2,
}

struct Running {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<ServeReport>,
}

impl Running {
    async fn report(mut self) -> ServeReport {
        let _ = self.stop.take().unwrap().send(());
        self.task.await.unwrap()
    }
}

async fn origin(kind: Origin, tls: bool) -> Running {
    let http2 = matches!(kind, Origin::Http2).then(|| {
        Settings::from(&Http2 {
            cleartext: true,
            ..Http2::default()
        })
    });
    let limits = ServerLimits {
        http2,
        drain_timeout: Duration::from_secs(1),
        ..ServerLimits::default()
    };
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let until = async move {
        let _ = stopped.await;
    };
    let task = if tls {
        let shared = Shared {
            http2: http2.is_some(),
            ..Shared::default()
        };
        let snapshot =
            Snapshot::load_with(&tlsc::tls_config(vec![tlsc::cert("exact", &[])]), &shared)
                .unwrap();
        let acceptor = Acceptor::new(Store::new(Arc::new(snapshot)), None);
        tokio::spawn(serve_tls(
            socket,
            acceptor,
            Arc::new(Version),
            limits,
            until,
        ))
    } else {
        tokio::spawn(serve(socket, Arc::new(Version), limits, until))
    };
    Running {
        addr,
        stop: Some(stop),
        task,
    }
}

fn config(up: SocketAddr, protocol: &str, tls: bool) -> Config {
    let mut cluster = serde_json::json!({
        "name": "web",
        "endpoints": [{"address": up.to_string()}],
        "protocol": protocol,
    });
    if tls {
        cluster["tls"] = serde_json::json!({
            "ca_file": tlsc::fixture("ca.crt"),
            "server_name": "api.example.com",
        });
    }
    let js = serde_json::json!({
        "listen": "127.0.0.1:0",
        "routes": [{"cluster": "web"}],
        "clusters": [cluster],
    });
    Config::from_json(js.to_string().as_bytes()).unwrap()
}

async fn proxy(config: &Config) -> SocketAddr {
    let p = Arc::new(Proxy::new(config));
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(serve(
        socket,
        p,
        ServerLimits::default(),
        std::future::pending(),
    ));
    addr
}

/// A POST with a 5-byte body through the proxy, over HTTP/1.1.
async fn post(addr: SocketAddr) -> (u16, String) {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = http::Request::post("/x")
        .header("host", "app.test")
        .body(Full::new(Bytes::from_static(b"hello")))
        .unwrap();
    let resp = send.send_request(req).await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn every_protocol_against_every_endpoint() {
    // (cluster protocol, origin, tls) and the version the origin saw, or
    // 502.
    let cases = [
        ("http1", Origin::Http1, false, Some("HTTP/1.1")),
        ("http1", Origin::Http2, false, Some("HTTP/1.1")),
        ("http1", Origin::Http1, true, Some("HTTP/1.1")),
        ("http1", Origin::Http2, true, Some("HTTP/1.1")),
        ("http2", Origin::Http1, false, None),
        ("http2", Origin::Http2, false, Some("HTTP/2.0")),
        ("http2", Origin::Http1, true, None),
        ("http2", Origin::Http2, true, Some("HTTP/2.0")),
        ("auto", Origin::Http1, false, Some("HTTP/1.1")),
        ("auto", Origin::Http2, false, Some("HTTP/2.0")),
        ("auto", Origin::Http1, true, Some("HTTP/1.1")),
        ("auto", Origin::Http2, true, Some("HTTP/2.0")),
    ];
    for (protocol, kind, tls, want) in cases {
        let up = origin(kind, tls).await;
        let addr = proxy(&config(up.addr, protocol, tls)).await;
        let (status, body) = post(addr).await;
        let case = format!("{protocol} to {kind:?} (tls {tls}): {status} {body}");
        match want {
            Some(v) => {
                assert_eq!(status, 200, "{case}");
                // The endpoint sees the client's host, as :authority over
                // HTTP/2 and as Host over HTTP/1.1, and the whole body.
                assert_eq!(body, format!("{v} app.test 5"), "{case}");
            }
            None => assert_eq!(status, 502, "{case}"),
        }
        up.report().await;
    }
}

#[tokio::test]
async fn auto_downgrades_once_and_keeps_http11() {
    for tls in [false, true] {
        let up = origin(Origin::Http1, tls).await;
        let addr = proxy(&config(up.addr, "auto", tls)).await;
        for _ in 0..5 {
            let (status, body) = post(addr).await;
            assert_eq!((status, body.as_str()), (200, "HTTP/1.1 app.test 5"));
        }
        // One connection tried HTTP/2; the five requests then shared one
        // pooled HTTP/1.1 connection.
        let report = up.report().await;
        assert_eq!(report.accepted, 2, "tls {tls}");
    }
}

#[tokio::test]
async fn http2_streams_share_one_connection_to_the_endpoint() {
    let up = origin(Origin::Http2, false).await;
    let addr = proxy(&config(up.addr, "http2", false)).await;
    let mut tasks = Vec::new();
    for _ in 0..20 {
        tasks.push(tokio::spawn(post(addr)));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), (200, "HTTP/2.0 app.test 5".to_owned()));
    }
    assert_eq!(up.report().await.accepted, 1);
}

#[tokio::test]
async fn a_get_without_a_body_stays_without_one() {
    let up = origin(Origin::Http2, false).await;
    let addr = proxy(&config(up.addr, "http2", false)).await;
    let tcp = TcpStream::connect(addr).await.unwrap();
    let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = http::Request::get("/")
        .header("host", "app.test")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = send.send_request(req).await.unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"HTTP/2.0 app.test 0");
    up.report().await;
}

#[test]
fn the_protocol_is_validated() {
    let js = |p: &str| {
        format!(
            r#"{{"listen":":80","routes":[{{"cluster":"a"}}],"clusters":[{{"name":"a","endpoints":[{{"address":"h:1"}}],"protocol":"{p}"}}]}}"#
        )
    };
    use edgefront_core::config::UpstreamProtocol as P;
    for (p, want) in [
        ("", P::Http1),
        ("http1", P::Http1),
        ("http2", P::Http2),
        ("auto", P::Auto),
    ] {
        let cfg = Config::from_json(js(p).as_bytes()).unwrap();
        assert_eq!(cfg.clusters[0].protocol, want);
    }
    let err = Config::from_json(js("h3").as_bytes())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("clusters[0].protocol: unknown protocol \"h3\"; use http1, http2 or auto"),
        "{err}"
    );
    // The default is left out of the printed configuration.
    let printed = serde_json::to_string(&Config::from_json(js("").as_bytes()).unwrap()).unwrap();
    assert!(!printed.contains("protocol"), "{printed}");
}
