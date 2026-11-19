//! The load generator's TLS client, for scenarios that load a target over
//! HTTPS (`[tls]` in the scenario).
//!
//! The client sends the scenario's server name as SNI and offers `h2` or
//! `http/1.1` as ALPN to match the scenario's protocol. It does not check
//! the target's certificate chain or name, because a benchmark target's
//! certificate is a test one and trusting it is not what is measured, but
//! it does check every handshake signature with ring, so the client does
//! the same public-key work a browser would. Each connection does one
//! full handshake; there is no session resumption, so every connection
//! costs the target a full handshake as a new client would.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, ring};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::load::Protocol;

/// Accepts any certificate, and checks the handshake signatures made with
/// its key.
#[derive(Debug)]
struct SignaturesOnly(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for SignaturesOnly {
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
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

/// A TLS client for one target.
#[derive(Clone)]
pub struct TlsClient {
    connector: TlsConnector,
    server_name: ServerName<'static>,
}

impl std::fmt::Debug for TlsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsClient")
            .field("server_name", &self.server_name)
            .finish_non_exhaustive()
    }
}

impl TlsClient {
    /// A client sending `server_name` as SNI and the ALPN `protocol` uses.
    pub fn new(server_name: &str, protocol: Protocol) -> Result<TlsClient, String> {
        let server_name = ServerName::try_from(server_name.to_owned())
            .map_err(|e| format!("{server_name:?}: {e}"))?;
        let provider: Arc<CryptoProvider> = Arc::new(ring::default_provider());
        let verifier = SignaturesOnly(provider.signature_verification_algorithms);
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        config.alpn_protocols = vec![match protocol {
            Protocol::Http1 => b"http/1.1".to_vec(),
            Protocol::Http2 => b"h2".to_vec(),
        }];
        config.resumption = rustls::client::Resumption::disabled();
        Ok(TlsClient {
            connector: TlsConnector::from(Arc::new(config)),
            server_name,
        })
    }

    /// The server name it sends.
    pub fn server_name(&self) -> String {
        self.server_name.to_str().into_owned()
    }

    /// Runs the handshake over `stream`.
    pub async fn connect(&self, stream: TcpStream) -> std::io::Result<TlsStream<TcpStream>> {
        self.connector
            .connect(self.server_name.clone(), stream)
            .await
    }
}
