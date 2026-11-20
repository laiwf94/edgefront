//! Proving control of a name to an ACME CA (RFC 8555 section 8).
//!
//! A [`Solver`] sets up one challenge type for a name and takes it down
//! again once the CA has decided:
//!
//! - [`Http01`]: the CA fetches
//!   `http://<name>/.well-known/acme-challenge/<token>` and expects the key
//!   authorization. [`Http01`] is a [`Handler`](crate::listener::Handler)
//!   for the plain listener on `acme.http_listen`; it answers only tokens
//!   it holds, with `text/plain`, and 404 for everything else.
//! - [`TlsAlpn01`] (RFC 8737): the CA connects to `<name>:443` with the ALPN
//!   protocol `acme-tls/1` and SNI `<name>`, and expects a self-signed
//!   certificate for the name with a critical `acmeIdentifier` extension
//!   holding the SHA-256 of the key authorization. The TLS listener's
//!   certificate resolver serves it from [`AlpnChallenges`] to exactly such
//!   handshakes, and the listener closes the connection after the
//!   handshake, so nothing else is ever served on `acme-tls/1`.
//! - [`Dns01`]: the CA looks up `_acme-challenge.<name>` TXT and expects the
//!   base64url SHA-256 of the key authorization. [`Dns01`] runs the
//!   configured hook command as `<command…> set <fqdn> <value>` before the
//!   challenge and `<command…> clear <fqdn> <value>` after, and fails the
//!   challenge when `set` exits non-zero or takes over a minute.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use http::{Request, Response, StatusCode, header};
use http_body_util::Full;
use hyper::body::Incoming;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};

use super::b64url;
use super::client::{AcmeError, Key, OID_ECDSA_SHA256, OID_SUBJECT_ALT_NAME, name, san};
use crate::listener::Handler;
use crate::tls::der::write;

/// A boxed future, so solvers can be trait objects.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Sets up and takes down one challenge type.
pub trait Solver: Send + Sync {
    /// The challenge type, as ACME names it: `http-01`, `tls-alpn-01` or
    /// `dns-01`.
    fn kind(&self) -> &'static str;
    /// Makes `domain`'s challenge answer with `key_auth`.
    fn prepare<'a>(
        &'a self,
        domain: &'a str,
        token: &'a str,
        key_auth: &'a str,
    ) -> BoxFuture<'a, Result<(), AcmeError>>;
    /// Removes what [`prepare`](Solver::prepare) set up.
    fn cleanup<'a>(
        &'a self,
        domain: &'a str,
        token: &'a str,
        key_auth: &'a str,
    ) -> BoxFuture<'a, ()>;
}

/// The path prefix HTTP-01 answers under.
pub const HTTP01_PREFIX: &str = "/.well-known/acme-challenge/";

/// The HTTP-01 responder: key authorizations by token.
#[derive(Debug, Default)]
pub struct Http01 {
    tokens: Mutex<HashMap<String, String>>,
}

impl Http01 {
    pub fn new() -> Arc<Http01> {
        Arc::new(Http01::default())
    }

    /// The answer for a request path, if it is a token held now.
    pub fn answer(&self, path: &str) -> Option<String> {
        let token = path.strip_prefix(HTTP01_PREFIX)?;
        self.tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(token)
            .cloned()
    }
}

impl Solver for Arc<Http01> {
    fn kind(&self) -> &'static str {
        "http-01"
    }

    fn prepare<'a>(
        &'a self,
        _: &'a str,
        token: &'a str,
        key_auth: &'a str,
    ) -> BoxFuture<'a, Result<(), AcmeError>> {
        Box::pin(async move {
            self.tokens
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(token.to_owned(), key_auth.to_owned());
            Ok(())
        })
    }

    fn cleanup<'a>(&'a self, _: &'a str, token: &'a str, _: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.tokens
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(token);
        })
    }
}

impl Handler for Http01 {
    type Body = Full<Bytes>;

    async fn call(self: Arc<Self>, req: Request<Incoming>, _: SocketAddr) -> Response<Full<Bytes>> {
        let found = (req.method() == http::Method::GET || req.method() == http::Method::HEAD)
            .then(|| self.answer(req.uri().path()))
            .flatten();
        match found {
            Some(key_auth) => {
                let mut resp = Response::new(Full::new(Bytes::from(key_auth)));
                resp.headers_mut().insert(
                    header::CONTENT_TYPE,
                    header::HeaderValue::from_static("text/plain"),
                );
                resp
            }
            None => {
                let mut resp = Response::new(Full::new(Bytes::from_static(b"not found\n")));
                *resp.status_mut() = StatusCode::NOT_FOUND;
                resp
            }
        }
    }
}

/// The ALPN protocol of TLS-ALPN-01.
pub const ACME_TLS_ALPN: &[u8] = b"acme-tls/1";

/// The DER of `1.3.6.1.5.5.7.1.31`, id-pe-acmeIdentifier.
const OID_ACME_IDENTIFIER: &[u8] = &[0x2b, 6, 1, 5, 5, 7, 1, 31];

/// TLS-ALPN-01 certificates the TLS listener serves, by name.
#[derive(Debug, Default)]
pub struct AlpnChallenges {
    certs: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl AlpnChallenges {
    pub fn new() -> Arc<AlpnChallenges> {
        Arc::new(AlpnChallenges::default())
    }

    /// The challenge certificate for `server_name`, if one is set up.
    pub fn get(&self, server_name: &str) -> Option<Arc<CertifiedKey>> {
        self.certs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&server_name.to_ascii_lowercase())
            .cloned()
    }
}

/// The TLS-ALPN-01 solver over [`AlpnChallenges`].
#[derive(Debug, Clone)]
pub struct TlsAlpn01(pub Arc<AlpnChallenges>);

/// A self-signed certificate for `domain` with the `acmeIdentifier`
/// extension holding `digest` (RFC 8737 section 3), and its key.
pub fn alpn_certificate(domain: &str, digest: &[u8]) -> Result<CertifiedKey, AcmeError> {
    let key = Key::generate()?;
    let now = SystemTime::now();
    let validity = write::sequence(&[
        &write::generalized_time(now - Duration::from_secs(3600)),
        &write::generalized_time(now + Duration::from_secs(7 * 86_400)),
    ]);
    let mut serial = [0u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut serial)
        .map_err(|_| AcmeError::State("no randomness".into()))?;
    serial[0] &= 0x7f;
    let extensions = write::explicit(
        3,
        &write::sequence(&[
            &write::sequence(&[
                &write::oid(OID_SUBJECT_ALT_NAME),
                &write::octet_string(&san(&[domain.to_owned()])),
            ]),
            &write::sequence(&[
                &write::oid(OID_ACME_IDENTIFIER),
                &write::boolean(true),
                &write::octet_string(&write::octet_string(digest)),
            ]),
        ]),
    );
    let algorithm = write::sequence(&[&write::oid(OID_ECDSA_SHA256)]);
    let subject = name(domain);
    let tbs = write::sequence(&[
        &write::explicit(0, &write::unsigned(&[2])),
        &write::unsigned(&serial),
        &algorithm,
        &subject,
        &validity,
        &subject,
        &key.spki(),
        &extensions,
    ]);
    let signature = key.sign_der(&tbs)?;
    let cert = write::sequence(&[&tbs, &algorithm, &write::bit_string(&signature)]);
    let der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.pkcs8().to_vec()));
    // Not `CertifiedKey::from_der`: its key check parses the certificate
    // with WebPKI, which refuses the critical acmeIdentifier extension
    // that is the point of this certificate.
    let signing = rustls::crypto::ring::default_provider()
        .key_provider
        .load_private_key(der)
        .map_err(|e| AcmeError::Challenge(format!("tls-alpn-01 key: {e}")))?;
    Ok(CertifiedKey::new(vec![CertificateDer::from(cert)], signing))
}

impl Solver for TlsAlpn01 {
    fn kind(&self) -> &'static str {
        "tls-alpn-01"
    }

    fn prepare<'a>(
        &'a self,
        domain: &'a str,
        _: &'a str,
        key_auth: &'a str,
    ) -> BoxFuture<'a, Result<(), AcmeError>> {
        Box::pin(async move {
            let cert = alpn_certificate(domain, &Sha256::digest(key_auth.as_bytes()))?;
            self.0
                .certs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(domain.to_ascii_lowercase(), Arc::new(cert));
            Ok(())
        })
    }

    fn cleanup<'a>(&'a self, domain: &'a str, _: &'a str, _: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.0
                .certs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&domain.to_ascii_lowercase());
        })
    }
}

/// The DNS-01 hook: a command and its leading arguments.
#[derive(Debug, Clone)]
pub struct Dns01 {
    command: Vec<String>,
}

/// How long the hook may take.
pub const DNS_HOOK_TIMEOUT: Duration = Duration::from_secs(60);

impl Dns01 {
    pub fn new(command: Vec<String>) -> Dns01 {
        Dns01 { command }
    }

    /// The TXT record name and value for a challenge.
    pub fn record(domain: &str, key_auth: &str) -> (String, String) {
        let base = domain.strip_prefix("*.").unwrap_or(domain);
        (
            format!("_acme-challenge.{base}."),
            b64url(&Sha256::digest(key_auth.as_bytes())),
        )
    }

    async fn run(&self, action: &str, fqdn: &str, value: &str) -> Result<(), AcmeError> {
        let (program, args) = self
            .command
            .split_first()
            .ok_or_else(|| AcmeError::Challenge("dns_hook is empty".into()))?;
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .arg(action)
            .arg(fqdn)
            .arg(value)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let out = tokio::time::timeout(DNS_HOOK_TIMEOUT, cmd.output())
            .await
            .map_err(|_| {
                AcmeError::Challenge(format!("dns_hook {action} {fqdn}: no exit within 60s"))
            })?
            .map_err(|e| AcmeError::Challenge(format!("dns_hook {program}: {e}")))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(AcmeError::Challenge(format!(
                "dns_hook {action} {fqdn}: {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
    }
}

impl Solver for Dns01 {
    fn kind(&self) -> &'static str {
        "dns-01"
    }

    fn prepare<'a>(
        &'a self,
        domain: &'a str,
        _: &'a str,
        key_auth: &'a str,
    ) -> BoxFuture<'a, Result<(), AcmeError>> {
        Box::pin(async move {
            let (fqdn, value) = Dns01::record(domain, key_auth);
            self.run("set", &fqdn, &value).await
        })
    }

    fn cleanup<'a>(&'a self, domain: &'a str, _: &'a str, key_auth: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let (fqdn, value) = Dns01::record(domain, key_auth);
            if let Err(e) = self.run("clear", &fqdn, &value).await {
                tracing::warn!(error = %e, "dns-01 record not cleared");
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::der;

    #[tokio::test]
    async fn http01_answers_only_held_tokens() {
        let h = Http01::new();
        assert_eq!(h.kind(), "http-01");
        h.prepare("edge.test", "tok", "tok.thumb").await.unwrap();
        assert_eq!(
            h.answer("/.well-known/acme-challenge/tok").as_deref(),
            Some("tok.thumb")
        );
        assert_eq!(h.answer("/.well-known/acme-challenge/other"), None);
        assert_eq!(h.answer("/tok"), None);
        h.cleanup("edge.test", "tok", "tok.thumb").await;
        assert_eq!(h.answer("/.well-known/acme-challenge/tok"), None);
    }

    #[tokio::test]
    async fn the_alpn_certificate_carries_the_digest() {
        let c = AlpnChallenges::new();
        let s = TlsAlpn01(Arc::clone(&c));
        assert_eq!(s.kind(), "tls-alpn-01");
        s.prepare("Edge.Test", "tok", "tok.thumb").await.unwrap();
        let cert = c.get("edge.test").unwrap();
        let tbs = der::tbs(&cert.cert[0]).unwrap();
        let (oid, critical, value) = tbs
            .extensions
            .iter()
            .find(|(o, _, _)| *o == OID_ACME_IDENTIFIER)
            .copied()
            .unwrap();
        assert_eq!(oid, OID_ACME_IDENTIFIER);
        assert!(critical);
        let digest = Sha256::digest(b"tok.thumb");
        assert_eq!(value, write::octet_string(&digest).as_slice());
        assert_eq!(
            der::cert_fields(&cert.cert[0]).unwrap().dns_names,
            vec!["Edge.Test".to_owned()]
        );
        // Self-signed: the signature verifies with its own key.
        assert!(crate::tls::ocsp::verify(
            tbs.signature_algorithm,
            tbs.key_algorithm,
            tbs.public_key,
            tbs.signed,
            tbs.signature
        ));
        s.cleanup("edge.test", "tok", "").await;
        assert!(c.get("edge.test").is_none());
    }

    #[tokio::test]
    async fn the_dns_hook_runs_set_and_clear() {
        let dir = std::env::temp_dir().join(format!("edgefront-dns01-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        let hook = Dns01::new(vec![
            "sh".into(),
            "-c".into(),
            format!("echo \"$@\" >> {}", log.display()),
            "hook".into(),
        ]);
        assert_eq!(hook.kind(), "dns-01");
        hook.prepare("*.edge.test", "tok", "tok.thumb")
            .await
            .unwrap();
        hook.cleanup("*.edge.test", "tok", "tok.thumb").await;
        let (fqdn, value) = Dns01::record("*.edge.test", "tok.thumb");
        assert_eq!(fqdn, "_acme-challenge.edge.test.");
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            format!("set {fqdn} {value}\nclear {fqdn} {value}\n")
        );
        let failing = Dns01::new(vec![
            "sh".into(),
            "-c".into(),
            "echo nope >&2; exit 3".into(),
            "x".into(),
        ]);
        let err = failing
            .prepare("edge.test", "t", "k")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope"), "{err}");
        let missing = Dns01::new(vec!["/nonexistent/hook".into()]);
        assert!(missing.prepare("edge.test", "t", "k").await.is_err());
        assert!(
            Dns01::new(Vec::new())
                .prepare("edge.test", "t", "k")
                .await
                .is_err()
        );
        missing.cleanup("edge.test", "t", "k").await;
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
