//! TLS and mutual TLS from the proxy to a cluster's endpoints: the pinned
//! CA and server name are checked, the client certificate is presented,
//! connections are pooled, a reload rotates the client certificate, and a
//! cluster whose TLS cannot be loaded refuses rather than speak cleartext.

#[path = "../../edgefront-core/tests/common/tls.rs"]
mod tlsc;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use edgefront_core::config::{ClientAuth, Config, UpstreamTls};
use edgefront_core::http1::TlsTo;
use edgefront_core::listener::{Handler, ServerLimits, serve, serve_tls};
use edgefront_core::tls::{Acceptor, Snapshot, Store, TlsInfo};
use edgefront_proxy::server::Proxy;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};

/// Answers with the client certificate's first URI or DNS name, or
/// `anonymous`.
struct Upstream;

impl Handler for Upstream {
    type Body = Full<Bytes>;

    async fn call(
        self: Arc<Self>,
        req: http::Request<Incoming>,
        _: SocketAddr,
    ) -> http::Response<Full<Bytes>> {
        let who = req
            .extensions()
            .get::<Arc<TlsInfo>>()
            .and_then(|t| t.client.as_ref())
            .and_then(|c| {
                c.uris
                    .first()
                    .cloned()
                    .or_else(|| c.dns_names.first().cloned())
            })
            .unwrap_or_else(|| "anonymous".to_owned());
        http::Response::new(Full::new(Bytes::from(who)))
    }
}

/// An HTTPS upstream with `cert`, requiring client certificates when
/// `mtls`.
async fn upstream(cert: &str, mtls: bool) -> (SocketAddr, Arc<AtomicUsize>) {
    let mut tls = tlsc::tls_config(vec![tlsc::cert(cert, &[])]);
    if mtls {
        tls.client_auth = Some(ClientAuth {
            ca_file: tlsc::fixture("ca.crt"),
            required: true,
        });
    }
    let connections = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&connections);
    let acceptor = Acceptor::new(
        Store::new(Arc::new(Snapshot::load(&tls).unwrap())),
        Some(Arc::new(move |r, _| {
            if r == edgefront_core::tls::HandshakeResult::Ok {
                c.fetch_add(1, Ordering::SeqCst);
            }
        })),
    );
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(serve_tls(
        socket,
        acceptor,
        Arc::new(Upstream),
        ServerLimits::default(),
        std::future::pending(),
    ));
    (addr, connections)
}

fn tls_block(server_name: Option<&str>, client: Option<&str>, ca: &str) -> serde_json::Value {
    let mut t = serde_json::json!({ "ca_file": tlsc::fixture(ca) });
    if let Some(n) = server_name {
        t["server_name"] = n.into();
    }
    if let Some(c) = client {
        t["cert_file"] = tlsc::fixture(&format!("{c}.crt")).into();
        t["key_file"] = tlsc::fixture(&format!("{c}.key")).into();
    }
    t
}

fn config(up: SocketAddr, tls: serde_json::Value) -> Config {
    let js = serde_json::json!({
        "listen": "127.0.0.1:0",
        "routes": [{"cluster": "web"}],
        "clusters": [{"name": "web", "endpoints": [{"address": up.to_string()}], "tls": tls}],
    });
    Config::from_json(js.to_string().as_bytes()).unwrap()
}

/// A proxy serving `config` on a free port.
async fn proxy(config: &Config) -> (SocketAddr, Arc<Proxy>) {
    let p = Arc::new(Proxy::new(config));
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(serve(
        socket,
        Arc::clone(&p),
        ServerLimits::default(),
        std::future::pending(),
    ));
    (addr, p)
}

async fn get(addr: SocketAddr) -> (u16, String) {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = http::Request::get("/")
        .header("host", "edge.test")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = send.send_request(req).await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn tls_to_the_upstream_checks_the_pinned_ca_and_name() {
    let (up, connections) = upstream("exact", false).await;
    // The endpoint is an IP, so the certificate's name comes from
    // server_name.
    let (edge, _p) = proxy(&config(
        up,
        tls_block(Some("api.example.com"), None, "ca.crt"),
    ))
    .await;
    for _ in 0..5 {
        assert_eq!(get(edge).await, (200, "anonymous".to_owned()));
    }
    // One pooled connection served all five.
    assert_eq!(connections.load(Ordering::SeqCst), 1);

    // Without the name the IP does not match the certificate; with another
    // CA pinned the chain does not verify. Both are refused as connect
    // errors (502), never sent in cleartext.
    for tls in [
        tls_block(None, None, "ca.crt"),
        tls_block(Some("api.example.com"), None, "other-ca.crt"),
        tls_block(Some("wrong.example.com"), None, "ca.crt"),
    ] {
        let (edge, _p) = proxy(&config(up, tls.clone())).await;
        assert_eq!(get(edge).await.0, 502, "{tls}");
    }
}

#[tokio::test]
async fn mutual_tls_presents_and_rotates_the_client_certificate() {
    let (up, _) = upstream("exact", true).await;
    // No client certificate: the upstream refuses the handshake.
    let (edge, _p) = proxy(&config(
        up,
        tls_block(Some("api.example.com"), None, "ca.crt"),
    ))
    .await;
    assert_eq!(get(edge).await.0, 502);

    let (edge, p) = proxy(&config(
        up,
        tls_block(Some("api.example.com"), Some("client"), "ca.crt"),
    ))
    .await;
    assert_eq!(
        get(edge).await,
        (200, "spiffe://edge.test/ns/web/sa/frontend".to_owned())
    );
    // A reload re-reads the client certificate: rotate to another one the
    // upstream also trusts.
    let rotated = config(
        up,
        tls_block(Some("api.example.com"), Some("client-rotated"), "ca.crt"),
    );
    p.reload(&rotated).unwrap();
    assert_eq!(
        get(edge).await,
        (200, "spiffe://edge.test/ns/web/sa/rotated".to_owned())
    );
    // An expired client certificate is refused by the upstream.
    p.reload(&config(
        up,
        tls_block(Some("api.example.com"), Some("client-expired"), "ca.crt"),
    ))
    .unwrap();
    assert_eq!(get(edge).await.0, 502);
}

#[tokio::test]
async fn unreadable_tls_refuses_rather_than_fall_back() {
    let (up, _) = upstream("exact", false).await;
    let missing = config(up, tls_block(Some("api.example.com"), None, "absent.pem"));
    let (edge, _p) = proxy(&missing).await;
    assert_eq!(get(edge).await.0, 502);

    let load = |t: UpstreamTls| TlsTo::load(&t).unwrap_err();
    let base = UpstreamTls {
        ca_file: tlsc::fixture("ca.crt"),
        server_name: None,
        cert_file: None,
        key_file: None,
    };
    assert!(
        load(UpstreamTls {
            ca_file: tlsc::fixture("garbage.pem"),
            ..base.clone()
        })
        .contains("no CA certificates")
    );
    assert!(
        load(UpstreamTls {
            cert_file: Some(tlsc::fixture("client.crt")),
            key_file: Some(tlsc::fixture("exact.key")),
            ..base.clone()
        })
        .contains("client.crt and")
    );
    assert!(
        load(UpstreamTls {
            cert_file: Some(tlsc::fixture("absent.crt")),
            key_file: Some(tlsc::fixture("client.key")),
            ..base.clone()
        })
        .contains("absent.crt")
    );
    assert!(TlsTo::load(&base).is_ok());
}

#[test]
fn config_checks_the_upstream_tls_block() {
    let parse = |tls: &str| {
        Config::from_json(
            format!(
                r#"{{"listen":":1","routes":[{{"cluster":"a"}}],
                "clusters":[{{"name":"a","endpoints":[{{"address":"h:1"}}],"tls":{tls}}}]}}"#
            )
            .as_bytes(),
        )
    };
    let c = parse(r#"{"ca_file":"/ca.pem","server_name":"api.example.com"}"#).unwrap();
    let t = c.clusters[0].tls.as_ref().unwrap();
    assert_eq!(t.server_name.as_deref(), Some("api.example.com"));
    for (tls, want) in [
        ("{}", "clusters[0].tls.ca_file: is required"),
        (
            r#"{"ca_file":"/ca","cert_file":"/c"}"#,
            "cert_file and key_file go together",
        ),
        (r#"{"ca_file":"/ca","server_name":"-x-"}"#, "server_name"),
        (r#"{"ca_file":"/ca","verify":false}"#, "clusters[0].tls"),
    ] {
        let err = parse(tls).unwrap_err().to_string();
        assert!(err.contains(want), "{tls}: {err}");
    }
}
