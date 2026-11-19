//! TLS test helpers, shared by the crates' tests through `#[path]`: the
//! certificates in `crates/edgefront-core/tests/fixtures/tls` (steer's
//! tlsedge test set, written by `gen.go` there), `tls` blocks built from
//! them, and a rustls client to handshake and send requests with.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use edgefront_core::config::{Certificate, Tls, TlsVersion};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::TokioIo;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

/// The path of a fixture file. The path goes through `crates/`, so the
/// helpers work from any crate's tests that include this file.
pub fn fixture(name: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../edgefront-core/tests/fixtures/tls");
    p.push(name);
    p.to_string_lossy().into_owned()
}

/// The `tls.certificates` entry for a fixture pair, with `names` as its
/// `server_names` when given.
pub fn cert(base: &str, names: &[&str]) -> Certificate {
    Certificate {
        cert_file: fixture(&format!("{base}.crt")),
        key_file: fixture(&format!("{base}.key")),
        server_names: if names.is_empty() {
            None
        } else {
            Some(names.iter().map(|n| (*n).to_owned()).collect())
        },
    }
}

/// A `tls` block as steer's tests built it: TLS 1.2 minimum with the
/// modern suites, a 2 s handshake timeout and 64 handshakes at once.
pub fn tls_config(certs: Vec<Certificate>) -> Tls {
    let modern = [0xc02b, 0xc02f, 0xc02c, 0xc030, 0xcca9, 0xcca8];
    Tls {
        certificates: certs,
        min_version: TlsVersion::Tls12,
        cipher_suites: Some(modern.iter().map(|&id| suite(id)).collect()),
        handshake_timeout: Duration::from_secs(2),
        max_concurrent_handshakes: 64,
        strict_sni: false,
        ocsp: Default::default(),
    }
}

/// A config's cipher suite by IANA number, through the config parser so
/// the names stay the config's own.
pub fn suite(id: u16) -> edgefront_core::config::CipherSuite {
    let name = match id {
        0xc009 => "TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA",
        0xc013 => "TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA",
        0xc02b => "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
        0xc02c => "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
        0xc02f => "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
        0xc030 => "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
        0xcca8 => "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
        0xcca9 => "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
        _ => panic!("no test name for suite {id:#x}"),
    };
    edgefront_core::config::CipherSuite { id, name }
}

/// steer's standard fixture block: default, exact, wild and the dual
/// ECDSA and RSA pair, in that order.
pub fn standard() -> Tls {
    tls_config(vec![
        cert("default", &[]),
        cert("exact", &[]),
        cert("wild", &[]),
        cert("dual-ecdsa", &[]),
        cert("dual-rsa", &[]),
    ])
}

/// The leaf certificate of a fixture.
pub fn leaf(base: &str) -> CertificateDer<'static> {
    CertificateDer::from_pem_file(fixture(&format!("{base}.crt"))).unwrap()
}

/// Which fixture a presented leaf is.
pub fn which(presented: &CertificateDer<'_>) -> &'static str {
    for base in [
        "default",
        "exact",
        "wild",
        "dual-ecdsa",
        "dual-rsa",
        "rotated",
        "nonames",
        "expired",
        "future",
        "other",
    ] {
        if leaf(base).as_ref() == presented.as_ref() {
            return base;
        }
    }
    "unknown"
}

/// How the test client handshakes.
#[derive(Clone)]
pub struct ClientOpts {
    /// Trust anchors file; `None` accepts any certificate (for no SNI).
    pub roots: Option<&'static str>,
    pub max_tls12: bool,
    /// TLS 1.2 suites to offer instead of ring's defaults.
    pub tls12_suites: Option<Vec<rustls::SupportedCipherSuite>>,
    pub alpn: Vec<Vec<u8>>,
}

impl Default for ClientOpts {
    fn default() -> Self {
        ClientOpts {
            roots: Some("ca.crt"),
            max_tls12: false,
            tls12_suites: None,
            alpn: vec![b"http/1.1".to_vec()],
        }
    }
}

#[derive(Debug)]
struct AcceptAny;

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A rustls client configuration for `opts`.
pub fn client_config(opts: &ClientOpts) -> Arc<ClientConfig> {
    let mut provider = ring::default_provider();
    if let Some(s) = &opts.tls12_suites {
        provider.cipher_suites = s.clone();
    }
    let provider: Arc<CryptoProvider> = Arc::new(provider);
    let versions: &[&rustls::SupportedProtocolVersion] = if opts.max_tls12 {
        &[&rustls::version::TLS12]
    } else {
        &[&rustls::version::TLS13, &rustls::version::TLS12]
    };
    let builder = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .unwrap();
    let mut cfg = match opts.roots {
        Some(file) => {
            let mut roots = RootCertStore::empty();
            for c in CertificateDer::pem_file_iter(fixture(file)).unwrap() {
                roots.add(c.unwrap()).unwrap();
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
        None => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny))
            .with_no_client_auth(),
    };
    cfg.alpn_protocols = opts.alpn.clone();
    Arc::new(cfg)
}

/// Connects to `addr` and handshakes with SNI `sni` (none when empty).
pub async fn handshake(
    addr: std::net::SocketAddr,
    sni: &str,
    opts: &ClientOpts,
) -> std::io::Result<TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(addr).await?;
    let name = if sni.is_empty() {
        ServerName::IpAddress(addr.ip().into())
    } else {
        ServerName::try_from(sni.to_owned()).unwrap()
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        tokio_rustls::TlsConnector::from(client_config(opts)).connect(name, tcp),
    )
    .await
    .map_err(|_| std::io::Error::other("client handshake timed out"))?
}

/// The fixture the server presented on an established stream.
pub fn presented(s: &TlsStream<TcpStream>) -> &'static str {
    which(&s.get_ref().1.peer_certificates().unwrap()[0])
}

/// An HTTP/1.1 client over one TLS connection.
pub struct Client {
    send: hyper::client::conn::http1::SendRequest<Empty<Bytes>>,
    pub cert: &'static str,
}

impl Client {
    pub async fn connect(addr: std::net::SocketAddr, sni: &str, opts: &ClientOpts) -> Client {
        let s = handshake(addr, sni, opts).await.unwrap();
        let cert = presented(&s);
        let (send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(s))
            .await
            .unwrap();
        tokio::spawn(conn);
        Client { send, cert }
    }

    /// Sends `GET path` and returns the status and body.
    pub async fn get(&mut self, path: &str) -> Result<(u16, String), hyper::Error> {
        self.send.ready().await?;
        let req = http::Request::get(path)
            .header("host", "edge.test")
            .body(Empty::new())
            .unwrap();
        let resp = self.send.send_request(req).await?;
        let status = resp.status().as_u16();
        let body = resp.into_body().collect().await?.to_bytes();
        Ok((status, String::from_utf8_lossy(&body).into_owned()))
    }

    /// Whether the connection can take another request.
    pub fn is_open(&self) -> bool {
        !self.send.is_closed()
    }
}

/// Connects with `cfg` (and its session cache), sends one request so the
/// server's tickets are read, and returns whether the handshake resumed.
pub async fn resumed(addr: std::net::SocketAddr, sni: &str, cfg: &Arc<ClientConfig>) -> bool {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let name = ServerName::try_from(sni.to_owned()).unwrap();
    let s = tokio_rustls::TlsConnector::from(Arc::clone(cfg))
        .connect(name, tcp)
        .await
        .unwrap();
    let kind = s.get_ref().1.handshake_kind();
    let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(s))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = http::Request::get("/")
        .header("host", "edge.test")
        .body(Empty::<Bytes>::new())
        .unwrap();
    assert_eq!(send.send_request(req).await.unwrap().status(), 200);
    kind == Some(rustls::HandshakeKind::Resumed)
}
