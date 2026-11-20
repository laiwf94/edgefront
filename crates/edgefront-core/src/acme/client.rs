//! The ACME protocol (RFC 8555): accounts, orders, authorizations,
//! finalization and the certificate download.
//!
//! Every request after the directory is a POST whose body is a flattened
//! JWS (section 6.2) signed ES256 with the account's P-256 key: the first,
//! `newAccount`, carries the public key as a JWK, and every later one the
//! account URL as `kid`. Each request uses the `Replay-Nonce` of the
//! answer before it, or a fresh one from `newNonce`; a `badNonce` problem
//! is retried with the nonce it came with, up to three times. Reading a
//! resource is a POST-as-GET (an empty payload). Responses are capped at
//! 1 MiB and each exchange at 30 seconds, and the directory's TLS
//! certificate must chain to the configured CA bundle.
//!
//! [`issue`] runs a whole order: `newOrder` for the names, each pending
//! authorization's challenge answered through a [`Solver`], the order
//! polled until ready, finalized with a CSR for a fresh P-256 key, polled
//! until valid, and the chain downloaded. [`AccountStore`] keeps the
//! account key and URL in the state directory at mode 0600, so a restart
//! reuses the account instead of creating another.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use http::{Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use ring::rand::SystemRandom;
use ring::signature::{
    ECDSA_P256_SHA256_ASN1_SIGNING, ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair,
};
use rustls::pki_types::ServerName;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;

use super::b64url;
use super::challenges::Solver;
use crate::tls::der::write;

/// The largest response read.
pub const MAX_RESPONSE_BYTES: usize = 1 << 20;
/// How long one exchange may take.
pub const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// An ACME failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcmeError {
    /// The exchange failed below ACME: connect, TLS, HTTP or a timeout.
    Transport(String),
    /// The server answered with a problem document (RFC 7807).
    Problem {
        status: u16,
        kind: String,
        detail: String,
    },
    /// The server's answer is not what the protocol says.
    Protocol(String),
    /// An authorization or the order became `invalid`.
    Invalid(String),
    /// Polling ran past its deadline.
    Timeout(String),
    /// The state directory could not be read or written.
    State(String),
    /// A challenge could not be set up.
    Challenge(String),
}

impl fmt::Display for AcmeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AcmeError::Transport(e) => write!(f, "acme transport: {e}"),
            AcmeError::Problem {
                status,
                kind,
                detail,
            } => write!(f, "acme problem {kind} ({status}): {detail}"),
            AcmeError::Protocol(e) => write!(f, "acme protocol: {e}"),
            AcmeError::Invalid(e) => write!(f, "acme: {e}"),
            AcmeError::Timeout(e) => write!(f, "acme: timed out {e}"),
            AcmeError::State(e) => write!(f, "acme state: {e}"),
            AcmeError::Challenge(e) => write!(f, "acme challenge: {e}"),
        }
    }
}

impl std::error::Error for AcmeError {}

fn protocol(e: impl fmt::Display) -> AcmeError {
    AcmeError::Protocol(e.to_string())
}

/// A P-256 key for signing: the account key or a certificate key.
pub struct Key {
    fixed: EcdsaKeyPair,
    asn1: EcdsaKeyPair,
    pkcs8: Vec<u8>,
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Key")
            .field("thumbprint", &self.thumbprint())
            .finish_non_exhaustive()
    }
}

impl Key {
    /// A new random key.
    pub fn generate() -> Result<Key, AcmeError> {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .map_err(|_| AcmeError::State("cannot generate a P-256 key".into()))?;
        Key::from_pkcs8(pkcs8.as_ref())
    }

    /// A key from its PKCS#8 DER.
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Key, AcmeError> {
        let rng = SystemRandom::new();
        let bad = |_| AcmeError::State("not a P-256 PKCS#8 key".into());
        Ok(Key {
            fixed: EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8, &rng)
                .map_err(bad)?,
            asn1: EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8, &rng)
                .map_err(bad)?,
            pkcs8: pkcs8.to_vec(),
        })
    }

    /// The PKCS#8 DER.
    pub fn pkcs8(&self) -> &[u8] {
        &self.pkcs8
    }

    /// The uncompressed public point, `04 || x || y`.
    fn point(&self) -> &[u8] {
        self.fixed.public_key().as_ref()
    }

    /// The public key as a JWK with its members in the order RFC 7638
    /// hashes them.
    pub fn jwk(&self) -> Value {
        let p = self.point();
        json!({"crv": "P-256", "kty": "EC", "x": b64url(&p[1..33]), "y": b64url(&p[33..65])})
    }

    /// The JWK thumbprint (RFC 7638): SHA-256 of the canonical JWK.
    pub fn thumbprint(&self) -> String {
        let p = self.point();
        let canonical = format!(
            r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
            b64url(&p[1..33]),
            b64url(&p[33..65])
        );
        b64url(&Sha256::digest(canonical.as_bytes()))
    }

    /// The key authorization for a challenge token (section 8.1).
    pub fn key_authorization(&self, token: &str) -> String {
        format!("{token}.{}", self.thumbprint())
    }

    /// An ES256 JWS signature: `r || s`, 64 bytes.
    fn sign_jws(&self, message: &[u8]) -> Result<Vec<u8>, AcmeError> {
        self.fixed
            .sign(&SystemRandom::new(), message)
            .map(|s| s.as_ref().to_vec())
            .map_err(|_| AcmeError::State("signing failed".into()))
    }

    /// An X.509 ECDSA signature: DER `SEQUENCE { r, s }`.
    pub fn sign_der(&self, message: &[u8]) -> Result<Vec<u8>, AcmeError> {
        self.asn1
            .sign(&SystemRandom::new(), message)
            .map(|s| s.as_ref().to_vec())
            .map_err(|_| AcmeError::State("signing failed".into()))
    }

    /// The SubjectPublicKeyInfo DER.
    pub fn spki(&self) -> Vec<u8> {
        write::sequence(&[
            &write::sequence(&[&write::oid(OID_EC_PUBLIC_KEY), &write::oid(OID_P256)]),
            &write::bit_string(self.point()),
        ])
    }
}

pub(crate) const OID_EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
pub(crate) const OID_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
pub(crate) const OID_ECDSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
pub(crate) const OID_COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
pub(crate) const OID_SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x11];
const OID_EXTENSION_REQUEST: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x0e];

/// A Name with one common name.
pub(crate) fn name(cn: &str) -> Vec<u8> {
    write::sequence(&[&write::set(&[&write::sequence(&[
        &write::oid(OID_COMMON_NAME),
        &write::utf8(cn),
    ])])])
}

/// The subjectAltName extension's value for DNS names.
pub(crate) fn san(domains: &[String]) -> Vec<u8> {
    let names: Vec<Vec<u8>> = domains
        .iter()
        .map(|d| write::implicit(2, d.as_bytes()))
        .collect();
    let names: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
    write::sequence(&names)
}

/// A PKCS#10 certificate signing request for `domains` with `key`: the
/// first domain as the common name, all of them as DNS SANs.
pub fn csr(domains: &[String], key: &Key) -> Result<Vec<u8>, AcmeError> {
    let first = domains
        .first()
        .ok_or_else(|| AcmeError::Protocol("no domains".into()))?;
    let extension = write::sequence(&[
        &write::oid(OID_SUBJECT_ALT_NAME),
        &write::octet_string(&san(domains)),
    ]);
    let attribute = write::sequence(&[
        &write::oid(OID_EXTENSION_REQUEST),
        &write::set(&[&write::sequence(&[&extension])]),
    ]);
    let info = write::sequence(&[
        &write::unsigned(&[0]),
        &name(first),
        &key.spki(),
        &write::explicit(0, &attribute),
    ]);
    let signature = key.sign_der(&info)?;
    Ok(write::sequence(&[
        &info,
        &write::sequence(&[&write::oid(OID_ECDSA_SHA256)]),
        &write::bit_string(&signature),
    ]))
}

/// The directory's endpoints.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Directory {
    pub new_nonce: String,
    pub new_account: String,
    pub new_order: String,
}

/// An order (section 7.1.3).
#[derive(Debug, Clone, Deserialize)]
pub struct Order {
    pub status: String,
    #[serde(default)]
    pub authorizations: Vec<String>,
    pub finalize: String,
    pub certificate: Option<String>,
}

/// An identifier.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Identifier {
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
}

/// An authorization (section 7.1.4).
#[derive(Debug, Clone, Deserialize)]
pub struct Authorization {
    pub status: String,
    pub identifier: Identifier,
    #[serde(default)]
    pub challenges: Vec<Challenge>,
    #[serde(default)]
    pub wildcard: bool,
}

/// A challenge (section 8).
#[derive(Debug, Clone, Deserialize)]
pub struct Challenge {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub token: String,
    pub status: String,
    pub error: Option<Value>,
}

/// An answer: status, the headers ACME reads, and the body.
#[derive(Debug, Clone)]
pub struct Answer {
    pub status: StatusCode,
    pub location: Option<String>,
    pub nonce: Option<String>,
    pub retry_after: Option<Duration>,
    pub body: Bytes,
}

impl Answer {
    fn json<T: for<'de> Deserialize<'de>>(&self) -> Result<T, AcmeError> {
        serde_json::from_slice(&self.body).map_err(protocol)
    }

    /// The problem document of an error answer.
    fn problem(&self) -> AcmeError {
        let v: Value = serde_json::from_slice(&self.body).unwrap_or(Value::Null);
        AcmeError::Problem {
            status: self.status.as_u16(),
            kind: v["type"].as_str().unwrap_or("about:blank").to_owned(),
            detail: v["detail"].as_str().unwrap_or("").to_owned(),
        }
    }
}

/// HTTPS to the CA, one connection per exchange.
#[derive(Clone)]
struct Https {
    tls: tokio_rustls::TlsConnector,
}

impl Https {
    async fn exchange(
        &self,
        method: Method,
        url: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Answer, AcmeError> {
        let work = async {
            let t = |e: &dyn fmt::Display| AcmeError::Transport(format!("{url}: {e}"));
            let uri: http::Uri = url.parse().map_err(|e| t(&e))?;
            if uri.scheme_str() != Some("https") {
                return Err(t(&"not an https:// URL"));
            }
            let host = uri.host().ok_or_else(|| t(&"no host"))?.to_owned();
            let port = uri.port_u16().unwrap_or(443);
            let bare = host.trim_matches(['[', ']']).to_owned();
            let tcp = TcpStream::connect((bare.as_str(), port))
                .await
                .map_err(|e| t(&e))?;
            let name = ServerName::try_from(bare).map_err(|e| t(&e))?;
            let tls = self.tls.connect(name, tcp).await.map_err(|e| t(&e))?;
            let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
                .await
                .map_err(|e| t(&e))?;
            tokio::spawn(conn);
            let path = uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
            let authority = uri.authority().map_or(host.clone(), |a| a.to_string());
            let mut req = Request::builder()
                .method(method)
                .uri(path)
                .header(header::HOST, authority)
                .header(header::USER_AGENT, "edgefront-acme/0.1");
            if body.is_some() {
                req = req.header(header::CONTENT_TYPE, "application/jose+json");
            }
            let req = req
                .body(Full::new(Bytes::from(body.unwrap_or_default())))
                .map_err(|e| t(&e))?;
            let resp = send.send_request(req).await.map_err(|e| t(&e))?;
            let head = |name: header::HeaderName| {
                resp.headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            };
            let status = resp.status();
            let location = head(header::LOCATION);
            let nonce = head(header::HeaderName::from_static("replay-nonce"));
            let retry_after = head(header::RETRY_AFTER)
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let body = Limited::new(resp.into_body(), MAX_RESPONSE_BYTES)
                .collect()
                .await
                .map_err(|e| t(&e))?
                .to_bytes();
            Ok(Answer {
                status,
                location,
                nonce,
                retry_after,
                body,
            })
        };
        tokio::time::timeout(EXCHANGE_TIMEOUT, work)
            .await
            .map_err(|_| AcmeError::Transport(format!("{url}: no answer within 30s")))?
    }
}

/// An ACME account on one CA.
pub struct Client {
    https: Https,
    directory: Directory,
    key: Arc<Key>,
    kid: Mutex<Option<String>>,
    nonce: Mutex<Option<String>>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("directory", &self.directory)
            .field("kid", &self.kid())
            .finish_non_exhaustive()
    }
}

/// The trust anchors in a PEM bundle, as a client configuration.
pub fn trust(ca_file: &str) -> Result<Arc<rustls::ClientConfig>, AcmeError> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    let bad = |e: &dyn fmt::Display| AcmeError::State(format!("{ca_file}: {e}"));
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(ca_file).map_err(|e| bad(&e))? {
        roots.add(cert.map_err(|e| bad(&e))?).map_err(|e| bad(&e))?;
    }
    if roots.is_empty() {
        return Err(bad(&"no CA certificates"));
    }
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| bad(&e))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

/// The system CA bundles tried when an ACME block names none, in order.
pub const SYSTEM_BUNDLES: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/cert.pem",
];

impl Client {
    /// Reads the directory at `url` over `trust`, for the account `key`.
    pub async fn connect(
        url: &str,
        trust: Arc<rustls::ClientConfig>,
        key: Arc<Key>,
        kid: Option<String>,
    ) -> Result<Client, AcmeError> {
        let https = Https {
            tls: tokio_rustls::TlsConnector::from(trust),
        };
        let answer = https.exchange(Method::GET, url, None).await?;
        if !answer.status.is_success() {
            return Err(answer.problem());
        }
        Ok(Client {
            https,
            directory: answer.json()?,
            key,
            kid: Mutex::new(kid),
            nonce: Mutex::new(None),
        })
    }

    /// The account URL, once the account exists.
    pub fn kid(&self) -> Option<String> {
        self.kid.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The account key.
    pub fn key(&self) -> &Arc<Key> {
        &self.key
    }

    async fn nonce(&self) -> Result<String, AcmeError> {
        if let Some(n) = self.nonce.lock().unwrap_or_else(|p| p.into_inner()).take() {
            return Ok(n);
        }
        let answer = self
            .https
            .exchange(Method::HEAD, &self.directory.new_nonce, None)
            .await?;
        answer
            .nonce
            .ok_or_else(|| AcmeError::Protocol("newNonce sent no Replay-Nonce".into()))
    }

    /// The flattened JWS for `payload` (`None` is POST-as-GET) at `url`.
    fn jws(&self, url: &str, nonce: &str, payload: Option<&Value>) -> Result<Vec<u8>, AcmeError> {
        let mut protected = json!({"alg": "ES256", "nonce": nonce, "url": url});
        match self.kid() {
            Some(kid) => protected["kid"] = kid.into(),
            None => protected["jwk"] = self.key.jwk(),
        }
        let protected = b64url(protected.to_string().as_bytes());
        let payload = payload.map_or(String::new(), |p| b64url(p.to_string().as_bytes()));
        let signature = self
            .key
            .sign_jws(format!("{protected}.{payload}").as_bytes())?;
        Ok(
            json!({"protected": protected, "payload": payload, "signature": b64url(&signature)})
                .to_string()
                .into_bytes(),
        )
    }

    /// A signed POST, retried on `badNonce`.
    pub async fn post(&self, url: &str, payload: Option<&Value>) -> Result<Answer, AcmeError> {
        let mut nonce = self.nonce().await?;
        for _ in 0..3 {
            let body = self.jws(url, &nonce, payload)?;
            let answer = self.https.exchange(Method::POST, url, Some(body)).await?;
            let fresh = answer.nonce.clone();
            if answer.status.is_success() {
                *self.nonce.lock().unwrap_or_else(|p| p.into_inner()) = fresh;
                return Ok(answer);
            }
            let problem = answer.problem();
            match (&problem, fresh) {
                (AcmeError::Problem { kind, .. }, Some(n))
                    if kind == "urn:ietf:params:acme:error:badNonce" =>
                {
                    nonce = n;
                }
                _ => return Err(problem),
            }
        }
        Err(AcmeError::Protocol(format!("{url}: badNonce three times")))
    }

    /// Creates the account, or finds it if the key already has one
    /// (section 7.3), and returns its URL.
    pub async fn account(&self, contact: &[String]) -> Result<String, AcmeError> {
        if let Some(kid) = self.kid() {
            return Ok(kid);
        }
        let answer = self
            .post(
                &self.directory.new_account.clone(),
                Some(&json!({"termsOfServiceAgreed": true, "contact": contact})),
            )
            .await?;
        let kid = answer
            .location
            .ok_or_else(|| AcmeError::Protocol("newAccount sent no Location".into()))?;
        *self.kid.lock().unwrap_or_else(|p| p.into_inner()) = Some(kid.clone());
        Ok(kid)
    }

    /// Places an order for `domains` and returns its URL and body.
    pub async fn new_order(&self, domains: &[String]) -> Result<(String, Order), AcmeError> {
        let ids: Vec<Value> = domains
            .iter()
            .map(|d| json!({"type": "dns", "value": d}))
            .collect();
        let answer = self
            .post(
                &self.directory.new_order.clone(),
                Some(&json!({ "identifiers": ids })),
            )
            .await?;
        let url = answer
            .location
            .clone()
            .ok_or_else(|| AcmeError::Protocol("newOrder sent no Location".into()))?;
        Ok((url, answer.json()?))
    }

    /// Reads a resource with POST-as-GET.
    pub async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
    ) -> Result<(T, Option<Duration>), AcmeError> {
        let answer = self.post(url, None).await?;
        Ok((answer.json()?, answer.retry_after))
    }

    /// Tells the CA a challenge is ready (section 7.5.1).
    pub async fn respond(&self, challenge: &str) -> Result<(), AcmeError> {
        self.post(challenge, Some(&json!({}))).await.map(drop)
    }

    /// Downloads a certificate chain as PEM.
    pub async fn download(&self, url: &str) -> Result<String, AcmeError> {
        let answer = self.post(url, None).await?;
        String::from_utf8(answer.body.to_vec()).map_err(protocol)
    }
}

/// An issued certificate.
#[derive(Debug, Clone)]
pub struct Issued {
    /// The chain, leaf first, PEM.
    pub chain_pem: String,
    /// The certificate's private key, PKCS#8 PEM.
    pub key_pem: String,
    pub not_before: SystemTime,
    pub not_after: SystemTime,
}

/// How often orders and authorizations are polled, unless the server's
/// `Retry-After` says otherwise, and how long in all.
pub const POLL_EVERY: Duration = Duration::from_secs(1);
pub const POLL_FOR: Duration = Duration::from_secs(120);

async fn poll<T, F>(client: &Client, url: &str, what: &str, mut done: F) -> Result<T, AcmeError>
where
    T: for<'de> Deserialize<'de>,
    F: FnMut(&T) -> Result<bool, AcmeError>,
{
    let deadline = tokio::time::Instant::now() + POLL_FOR;
    loop {
        let (value, retry_after): (T, _) = client.get(url).await?;
        if done(&value)? {
            return Ok(value);
        }
        let wait = retry_after
            .unwrap_or(POLL_EVERY)
            .clamp(Duration::from_millis(200), Duration::from_secs(10));
        if tokio::time::Instant::now() + wait > deadline {
            return Err(AcmeError::Timeout(format!("waiting for {what}")));
        }
        tokio::time::sleep(wait).await;
    }
}

/// Runs one order for `domains` to a certificate, answering challenges
/// with `solver`; see the module documentation.
pub async fn issue(
    client: &Client,
    contact: &[String],
    domains: &[String],
    solver: &dyn Solver,
) -> Result<Issued, AcmeError> {
    client.account(contact).await?;
    let (order_url, order) = client.new_order(domains).await?;
    for authz_url in &order.authorizations {
        let (authz, _): (Authorization, _) = client.get(authz_url).await?;
        if authz.status == "valid" {
            continue;
        }
        let challenge = authz
            .challenges
            .iter()
            .find(|c| c.kind == solver.kind())
            .ok_or_else(|| {
                AcmeError::Protocol(format!(
                    "{}: no {} challenge offered",
                    authz.identifier.value,
                    solver.kind()
                ))
            })?
            .clone();
        let key_auth = client.key().key_authorization(&challenge.token);
        let domain = authz.identifier.value.clone();
        solver.prepare(&domain, &challenge.token, &key_auth).await?;
        let result = async {
            client.respond(&challenge.url).await?;
            poll::<Authorization, _>(
                client,
                authz_url,
                &format!("{domain}'s authorization"),
                |a| match a.status.as_str() {
                    "valid" => Ok(true),
                    "pending" | "processing" => Ok(false),
                    other => {
                        let why = a
                            .challenges
                            .iter()
                            .find_map(|c| c.error.as_ref())
                            .map(|e| e["detail"].as_str().unwrap_or_default().to_owned())
                            .unwrap_or_default();
                        Err(AcmeError::Invalid(format!(
                            "{domain}'s authorization is {other}: {why}"
                        )))
                    }
                },
            )
            .await
        }
        .await;
        solver.cleanup(&domain, &challenge.token, &key_auth).await;
        result?;
    }
    poll::<Order, _>(client, &order_url, "the order to be ready", |o| {
        match o.status.as_str() {
            "ready" | "valid" | "processing" => Ok(true),
            "pending" => Ok(false),
            other => Err(AcmeError::Invalid(format!("the order is {other}"))),
        }
    })
    .await?;
    let key = Key::generate()?;
    let csr = csr(domains, &key)?;
    client
        .post(&order.finalize, Some(&json!({"csr": b64url(&csr)})))
        .await?;
    let done = poll::<Order, _>(client, &order_url, "the certificate", |o| {
        match o.status.as_str() {
            "valid" => Ok(true),
            "processing" | "ready" => Ok(false),
            other => Err(AcmeError::Invalid(format!("the order is {other}"))),
        }
    })
    .await?;
    let cert_url = done
        .certificate
        .ok_or_else(|| AcmeError::Protocol("a valid order with no certificate".into()))?;
    let chain_pem = client.download(&cert_url).await?;
    let leaf = {
        use rustls::pki_types::CertificateDer;
        use rustls::pki_types::pem::PemObject;
        CertificateDer::from_pem_slice(chain_pem.as_bytes()).map_err(protocol)?
    };
    let fields = crate::tls::der::cert_fields(&leaf).map_err(protocol)?;
    Ok(Issued {
        chain_pem,
        key_pem: super::pem("PRIVATE KEY", key.pkcs8()),
        not_before: fields.not_before,
        not_after: fields.not_after,
    })
}

/// The account and certificate files in a state directory: `account.key`
/// (PKCS#8 PEM), `account.json` (the account URL per directory), and
/// `cert.pem` and `key.pem` for the issued certificate. The directory is
/// made at mode 0700 and every file written at 0600, atomically (a
/// temporary file renamed over the old one).
#[derive(Debug, Clone)]
pub struct AccountStore {
    dir: PathBuf,
}

impl AccountStore {
    pub fn new(dir: impl Into<PathBuf>) -> AccountStore {
        AccountStore { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The issued certificate's chain file.
    pub fn cert_file(&self) -> PathBuf {
        self.dir.join("cert.pem")
    }

    /// The issued certificate's key file.
    pub fn key_file(&self) -> PathBuf {
        self.dir.join("key.pem")
    }

    fn ensure_dir(&self) -> Result<(), AcmeError> {
        let err = |e: std::io::Error| AcmeError::State(format!("{}: {e}", self.dir.display()));
        std::fs::create_dir_all(&self.dir).map_err(err)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))
                .map_err(err)?;
        }
        Ok(())
    }

    /// Writes `data` to `name` at mode 0600, atomically.
    pub fn write(&self, name: &str, data: &[u8]) -> Result<(), AcmeError> {
        use std::io::Write;
        self.ensure_dir()?;
        let path = self.dir.join(name);
        let tmp = self.dir.join(format!(".{name}.tmp"));
        let err = |e: std::io::Error| AcmeError::State(format!("{}: {e}", path.display()));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).map_err(err)?;
        f.write_all(data).map_err(err)?;
        f.sync_all().map_err(err)?;
        std::fs::rename(&tmp, &path).map_err(err)
    }

    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, AcmeError> {
        let path = self.dir.join(name);
        match std::fs::read(&path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(AcmeError::State(format!("{}: {e}", path.display()))),
        }
    }

    /// The account key, made and saved on first use.
    pub fn account_key(&self) -> Result<Key, AcmeError> {
        use rustls::pki_types::PrivatePkcs8KeyDer;
        use rustls::pki_types::pem::PemObject;
        match self.read("account.key")? {
            Some(pem) => {
                let der = PrivatePkcs8KeyDer::from_pem_slice(&pem)
                    .map_err(|e| AcmeError::State(format!("account.key: {e}")))?;
                Key::from_pkcs8(der.secret_pkcs8_der())
            }
            None => {
                let key = Key::generate()?;
                self.write(
                    "account.key",
                    super::pem("PRIVATE KEY", key.pkcs8()).as_bytes(),
                )?;
                Ok(key)
            }
        }
    }

    /// The account URL saved for `directory`.
    pub fn kid(&self, directory: &str) -> Result<Option<String>, AcmeError> {
        let Some(bytes) = self.read("account.json")? else {
            return Ok(None);
        };
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|e| AcmeError::State(format!("account.json: {e}")))?;
        Ok(v[directory].as_str().map(str::to_owned))
    }

    /// Saves the account URL for `directory`, keeping other directories'.
    pub fn save_kid(&self, directory: &str, kid: &str) -> Result<(), AcmeError> {
        let mut v: Value = match self.read("account.json")? {
            Some(b) => serde_json::from_slice(&b).unwrap_or_else(|_| json!({})),
            None => json!({}),
        };
        v[directory] = kid.into();
        self.write("account.json", v.to_string().as_bytes())
    }

    /// Saves an issued certificate and its key.
    pub fn save_issued(&self, issued: &Issued) -> Result<(), AcmeError> {
        self.write("key.pem", issued.key_pem.as_bytes())?;
        self.write("cert.pem", issued.chain_pem.as_bytes())
    }

    /// Whether an issued certificate is on disk.
    pub fn has_issued(&self) -> bool {
        self.cert_file().is_file() && self.key_file().is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::der;

    #[test]
    fn jwk_thumbprint_matches_rfc7638_form() {
        let k = Key::generate().unwrap();
        let jwk = k.jwk();
        assert_eq!(jwk["kty"], "EC");
        assert_eq!(jwk["x"].as_str().unwrap().len(), 43);
        let canonical = format!(
            r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
            jwk["x"].as_str().unwrap(),
            jwk["y"].as_str().unwrap()
        );
        assert_eq!(
            k.thumbprint(),
            b64url(&Sha256::digest(canonical.as_bytes()))
        );
        assert_eq!(
            k.key_authorization("tok"),
            format!("tok.{}", k.thumbprint())
        );
        let again = Key::from_pkcs8(k.pkcs8()).unwrap();
        assert_eq!(again.thumbprint(), k.thumbprint());
        assert!(Key::from_pkcs8(b"nope").is_err());
        assert!(format!("{k:?}").contains("thumbprint"));
    }

    #[test]
    fn the_csr_is_signed_and_names_every_domain() {
        let k = Key::generate().unwrap();
        let domains = vec!["edge.test".to_owned(), "www.edge.test".to_owned()];
        let csr = csr(&domains, &k).unwrap();
        let mut r = der::Reader::new(&csr);
        let mut req = r.sequence().unwrap();
        let info = der::whole(&mut req, der::tag::SEQUENCE).unwrap();
        let mut alg = req.sequence().unwrap();
        assert_eq!(alg.read(der::tag::OID).unwrap(), OID_ECDSA_SHA256);
        let sig = der::bits(req.read(der::tag::BIT_STRING).unwrap()).unwrap();
        let point = &k.spki()[k.spki().len() - 65..];
        ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, point)
            .verify(info, sig)
            .unwrap();
        // The SAN extension carries both names.
        let text = String::from_utf8_lossy(info);
        assert!(text.contains("edge.test") && text.contains("www.edge.test"));
        assert!(super::csr(&[], &k).is_err());
    }

    #[test]
    fn the_store_keeps_the_account_at_0600() {
        let dir = std::env::temp_dir().join(format!("edgefront-acme-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = AccountStore::new(dir.join("state"));
        let k1 = store.account_key().unwrap();
        let k2 = store.account_key().unwrap();
        assert_eq!(k1.thumbprint(), k2.thumbprint(), "the key is reused");
        assert_eq!(store.kid("https://ca/dir").unwrap(), None);
        store
            .save_kid("https://ca/dir", "https://ca/acct/1")
            .unwrap();
        store
            .save_kid("https://other/dir", "https://other/acct/9")
            .unwrap();
        assert_eq!(
            store.kid("https://ca/dir").unwrap().as_deref(),
            Some("https://ca/acct/1")
        );
        assert!(!store.has_issued());
        store
            .save_issued(&Issued {
                chain_pem: "chain".into(),
                key_pem: "key".into(),
                not_before: SystemTime::UNIX_EPOCH,
                not_after: SystemTime::UNIX_EPOCH,
            })
            .unwrap();
        assert!(store.has_issued());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in ["account.key", "account.json", "cert.pem", "key.pem"] {
                let mode = std::fs::metadata(store.dir().join(f))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{f}");
            }
            let mode = std::fs::metadata(store.dir()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        std::fs::write(store.dir().join("account.key"), "garbage").unwrap();
        assert!(store.account_key().is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn errors_say_what_failed() {
        for e in [
            AcmeError::Transport("x".into()),
            AcmeError::Problem {
                status: 400,
                kind: "urn:ietf:params:acme:error:malformed".into(),
                detail: "bad".into(),
            },
            AcmeError::Protocol("x".into()),
            AcmeError::Invalid("x".into()),
            AcmeError::Timeout("x".into()),
            AcmeError::State("x".into()),
            AcmeError::Challenge("x".into()),
        ] {
            assert!(e.to_string().starts_with("acme"), "{e}");
        }
    }
}
