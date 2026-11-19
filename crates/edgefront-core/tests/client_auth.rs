//! Client certificates on the TLS listener: a valid certificate from the
//! configured CA is let in and its SAN and SPIFFE ID reach the request; an
//! expired one, one from another CA, and none at all (when required) are
//! refused in the handshake; with `required = false` a client without a
//! certificate gets in with no identity.

#[path = "common/tls.rs"]
mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::*;
use edgefront_core::config::ClientAuth;
use edgefront_core::listener::{Handler, ServerLimits, serve_tls};
use edgefront_core::tls::{Acceptor, ClientIdentity, Snapshot, Store, TlsInfo};
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::net::{TcpListener, TcpStream};

/// Answers with the client's identity as `spiffe|dns,dns|uri,uri`.
struct Who;

impl Handler for Who {
    type Body = Full<Bytes>;

    async fn call(
        self: Arc<Self>,
        req: http::Request<Incoming>,
        _: SocketAddr,
    ) -> http::Response<Full<Bytes>> {
        let text = match req
            .extensions()
            .get::<Arc<TlsInfo>>()
            .and_then(|t| t.client.clone())
        {
            None => "anonymous".to_owned(),
            Some(c) => format!(
                "{}|{}|{}",
                c.spiffe_id.unwrap_or_default(),
                c.dns_names.join(","),
                c.uris.join(",")
            ),
        };
        http::Response::new(Full::new(Bytes::from(text)))
    }
}

async fn server(required: bool) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let mut tls = tls_config(vec![cert("exact", &[])]);
    tls.client_auth = Some(ClientAuth {
        ca_file: fixture("ca.crt"),
        required,
    });
    let store = Store::new(Arc::new(Snapshot::load(&tls).unwrap()));
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve_tls(
        socket,
        Acceptor::new(store, None),
        Arc::new(Who),
        ServerLimits::default(),
        async move {
            let _ = stopped.await;
        },
    ));
    (addr, stop)
}

/// A client trusting the fixture CA, presenting `client` if given.
fn client(client: Option<&str>) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_file(fixture("ca.crt")).unwrap())
        .unwrap();
    let builder =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots);
    Arc::new(match client {
        None => builder.with_no_client_auth(),
        Some(base) => builder
            .with_client_auth_cert(
                vec![CertificateDer::from_pem_file(fixture(&format!("{base}.crt"))).unwrap()],
                PrivateKeyDer::from_pem_file(fixture(&format!("{base}.key"))).unwrap(),
            )
            .unwrap(),
    })
}

/// One request; the body, or the error that ended the exchange (a
/// refusal shows when the client first reads, in TLS 1.3).
async fn who(addr: SocketAddr, cfg: Arc<ClientConfig>) -> Result<String, String> {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let name = ServerName::try_from("api.example.com").unwrap();
    let tls = tokio_rustls::TlsConnector::from(cfg)
        .connect(name, tcp)
        .await
        .map_err(|e| e.to_string())?;
    let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(conn);
    let req = http::Request::get("/")
        .header("host", "api.example.com")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = tokio::time::timeout(Duration::from_secs(5), send.send_request(req))
        .await
        .map_err(|_| "timed out".to_owned())?
        .map_err(|e| e.to_string())?;
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&body.to_bytes()).into_owned())
}

#[tokio::test]
async fn required_client_certificates() {
    let (addr, _stop) = server(true).await;
    assert_eq!(
        who(addr, client(Some("client"))).await.unwrap(),
        "spiffe://edge.test/ns/web/sa/frontend|client.example.com|spiffe://edge.test/ns/web/sa/frontend"
    );
    for (who_, name) in [
        (Some("client-expired"), "expired"),
        (Some("client-other"), "wrong CA"),
        (None, "none"),
    ] {
        let got = who(addr, client(who_)).await;
        assert!(got.is_err(), "{name}: {got:?}");
    }
    // A server certificate is not a client certificate: its key usage is
    // server authentication only.
    assert!(who(addr, client(Some("exact"))).await.is_err());
}

#[tokio::test]
async fn optional_client_certificates() {
    let (addr, _stop) = server(false).await;
    assert_eq!(who(addr, client(None)).await.unwrap(), "anonymous");
    assert!(
        who(addr, client(Some("client")))
            .await
            .unwrap()
            .starts_with("spiffe://edge.test/")
    );
    // Presenting a bad certificate is still refused.
    assert!(who(addr, client(Some("client-expired"))).await.is_err());
}

#[test]
fn identities_and_bad_bundles() {
    let leaf = CertificateDer::from_pem_file(fixture("client.crt")).unwrap();
    let id = ClientIdentity::of(&leaf);
    assert_eq!(
        id.spiffe_id.as_deref(),
        Some("spiffe://edge.test/ns/web/sa/frontend")
    );
    // A certificate without a SPIFFE URI has no SPIFFE ID.
    let id = ClientIdentity::of(&CertificateDer::from_pem_file(fixture("exact.crt")).unwrap());
    assert_eq!(id.spiffe_id, None);
    assert_eq!(id.dns_names, vec!["api.example.com".to_owned()]);
    assert_eq!(
        ClientIdentity::of(&CertificateDer::from(vec![1, 2, 3])),
        ClientIdentity::default()
    );

    for (file, want) in [
        (fixture("absent.pem"), "absent.pem"),
        (fixture("garbage.pem"), "no CA certificates"),
    ] {
        let mut tls = tls_config(vec![cert("exact", &[])]);
        tls.client_auth = Some(ClientAuth {
            ca_file: file,
            required: true,
        });
        let err = Snapshot::load(&tls).unwrap_err().to_string();
        assert!(
            err.contains("tls.client_auth.ca_file") && err.contains(want),
            "{err}"
        );
    }
}

#[test]
fn config_checks_client_auth_and_allowed_clients() {
    use edgefront_core::config::Config;
    let base = |cluster_extra: &str, tls_extra: &str| {
        format!(
            r#"{{"listen":":1","routes":[{{"cluster":"a"}}],
            "clusters":[{{"name":"a","endpoints":[{{"address":"h:1"}}]{cluster_extra}}}],
            "tls":{{"certificates":[{{"cert_file":"c","key_file":"k"}}]{tls_extra}}}}}"#
        )
    };
    let ok = Config::from_json(
        base(
            r#","allowed_clients":["spiffe://edge.test/ns/web/*","dns:client.example.com"]"#,
            r#","client_auth":{"ca_file":"/ca.pem"}"#,
        )
        .as_bytes(),
    )
    .unwrap();
    let tls = ok.tls.unwrap();
    assert_eq!(
        tls.client_auth,
        Some(ClientAuth {
            ca_file: "/ca.pem".into(),
            required: true
        })
    );
    assert_eq!(ok.clusters[0].allowed_clients.as_ref().unwrap().len(), 2);
    let optional = Config::from_json(
        base("", r#","client_auth":{"ca_file":"/ca.pem","required":false}"#).as_bytes(),
    )
    .unwrap();
    assert!(!optional.tls.unwrap().client_auth.unwrap().required);
    for (cluster, tls, want) in [
        (
            r#","allowed_clients":["spiffe://x/y"]"#,
            "",
            "clusters[0].allowed_clients: needs tls.client_auth",
        ),
        (
            r#","allowed_clients":[]"#,
            r#","client_auth":{"ca_file":"/ca.pem"}"#,
            "must not be empty",
        ),
        (
            r#","allowed_clients":["https://x/y"]"#,
            r#","client_auth":{"ca_file":"/ca.pem"}"#,
            "clusters[0].allowed_clients[0]",
        ),
        (
            r#","allowed_clients":["dns:-bad-"]"#,
            r#","client_auth":{"ca_file":"/ca.pem"}"#,
            "clusters[0].allowed_clients[0]",
        ),
        ("", r#","client_auth":{}"#, "tls.client_auth.ca_file: is required"),
        ("", r#","client_auth":{"ca_file":"x","bogus":1}"#, "tls.client_auth"),
    ] {
        let err = Config::from_json(base(cluster, tls).as_bytes())
            .unwrap_err()
            .to_string();
        assert!(err.contains(want), "{want}: {err}");
    }
}
