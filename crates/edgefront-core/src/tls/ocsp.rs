//! OCSP stapling (RFC 6960, RFC 6066 section 8): the edge fetches each
//! certificate's OCSP response itself and sends it in the handshake, so
//! clients learn the certificate is not revoked without asking the CA.
//!
//! For every certificate with an issuer in its chain and an OCSP URL (or
//! the listener's `ocsp_responder` override), [`Stapler`] sends an
//! `OCSPRequest` for its `CertID` (SHA-1 hashes of the issuer's name and
//! key, and the serial, as RFC 5019 requires of lightweight responders)
//! as an HTTP POST, and checks the answer before stapling it:
//!
//! - the response status is `successful` and the type is the basic one;
//! - it is signed by the issuer, or by a responder certificate it carries
//!   that the issuer signed and that has the OCSP signing key usage;
//! - it has a response for this `CertID` whose status is `good`;
//! - `thisUpdate` is not in the future and `nextUpdate`, if given, is not
//!   in the past (five minutes of slack each way for clock skew).
//!
//! A staple is refreshed halfway between its `thisUpdate` and
//! `nextUpdate` (after an hour when the responder gives no
//! `nextUpdate`), in the background, so no handshake ever waits on the
//! responder. A failed fetch keeps the current staple and retries after
//! a minute, doubling to an hour, with up to 10% jitter. A staple whose
//! `nextUpdate` has passed is withdrawn rather than served stale. Staples
//! live in [`Staples`], keyed by the certificate's SHA-256, which every
//! snapshot of a listener shares, so a reload staples at once from what
//! was already fetched.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use sha2::{Digest, Sha256};

use super::der::{self, Malformed, Reader, Tbs, tag, write};
use super::store::Store;
use crate::clock::Clock;

/// The largest OCSP response read.
pub const MAX_RESPONSE_BYTES: usize = 64 << 10;
/// How long one fetch may take.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Slack for clock skew on `thisUpdate` and `nextUpdate`.
pub const SKEW: Duration = Duration::from_secs(300);
/// The first retry after a failed fetch, doubled up to [`MAX_RETRY`].
pub const FIRST_RETRY: Duration = Duration::from_secs(60);
pub const MAX_RETRY: Duration = Duration::from_secs(3600);
/// When a response has no `nextUpdate`, it is refreshed after this long.
pub const NO_NEXT_UPDATE_REFRESH: Duration = Duration::from_secs(3600);

const OID_SHA1: &[u8] = &[0x2b, 0x0e, 0x03, 0x02, 0x1a];
const OID_OCSP_BASIC: &[u8] = &[0x2b, 6, 1, 5, 5, 7, 48, 1, 1];

/// Why a response was not stapled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OcspError {
    Malformed,
    /// The responder answered with a status other than `successful`.
    Status(u8),
    /// Not a basic response.
    NotBasic,
    /// Neither the issuer nor a delegated responder signed it.
    Signature,
    /// No response for this certificate.
    NoResponse,
    Revoked,
    Unknown,
    /// `thisUpdate` is in the future or `nextUpdate` has passed.
    Stale,
    /// The certificate has no issuer in its chain or no OCSP URL.
    NotStapleable(&'static str),
    Fetch(String),
}

impl fmt::Display for OcspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OcspError::Malformed => f.write_str("malformed OCSP response"),
            OcspError::Status(s) => write!(f, "OCSP responder status {s}"),
            OcspError::NotBasic => f.write_str("not a basic OCSP response"),
            OcspError::Signature => {
                f.write_str("OCSP response not signed by the issuer or a delegated responder")
            }
            OcspError::NoResponse => f.write_str("no OCSP response for the certificate"),
            OcspError::Revoked => f.write_str("the certificate is revoked"),
            OcspError::Unknown => f.write_str("the responder does not know the certificate"),
            OcspError::Stale => f.write_str("OCSP response outside its validity"),
            OcspError::NotStapleable(why) => f.write_str(why),
            OcspError::Fetch(e) => write!(f, "OCSP fetch failed: {e}"),
        }
    }
}

impl std::error::Error for OcspError {}

impl From<Malformed> for OcspError {
    fn from(_: Malformed) -> Self {
        OcspError::Malformed
    }
}

/// A certificate's OCSP `CertID`, with SHA-1 as RFC 5019 requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertId {
    pub issuer_name_hash: Vec<u8>,
    pub issuer_key_hash: Vec<u8>,
    pub serial: Vec<u8>,
}

impl CertId {
    /// The `CertID` of `leaf` as issued by `issuer`.
    pub fn new(leaf: &Tbs<'_>, issuer: &Tbs<'_>) -> CertId {
        let sha1 = |b: &[u8]| {
            ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, b)
                .as_ref()
                .to_vec()
        };
        CertId {
            issuer_name_hash: sha1(issuer.subject),
            issuer_key_hash: sha1(issuer.public_key),
            serial: leaf.serial.to_vec(),
        }
    }

    /// The DER `CertID`.
    pub fn encode(&self) -> Vec<u8> {
        write::sequence(&[
            &write::sequence(&[&write::oid(OID_SHA1), &write::null()]),
            &write::octet_string(&self.issuer_name_hash),
            &write::octet_string(&self.issuer_key_hash),
            &write::integer_raw(&self.serial),
        ])
    }

    /// The DER `OCSPRequest` for this certificate alone, unsigned and
    /// without a nonce (RFC 5019 responders ignore one).
    pub fn request(&self) -> Vec<u8> {
        let request = write::sequence(&[&self.encode()]);
        let list = write::sequence(&[&request]);
        let tbs = write::sequence(&[&list]);
        write::sequence(&[&tbs])
    }

    fn matches(&self, mut id: Reader<'_>) -> Result<bool, Malformed> {
        let mut alg = id.sequence()?;
        let oid = alg.read(tag::OID)?;
        let name = id.read(tag::OCTET_STRING)?;
        let key = id.read(tag::OCTET_STRING)?;
        let serial = id.read(tag::INTEGER)?;
        id.finish()?;
        Ok(oid == OID_SHA1
            && name == self.issuer_name_hash
            && key == self.issuer_key_hash
            && serial == self.serial)
    }
}

/// A checked OCSP response, ready to staple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staple {
    /// The whole `OCSPResponse`, as the handshake sends it.
    pub der: Vec<u8>,
    pub this_update: SystemTime,
    pub next_update: Option<SystemTime>,
}

impl Staple {
    /// When it should be refreshed: halfway through its validity.
    pub fn refresh_at(&self) -> SystemTime {
        match self.next_update {
            Some(next) => {
                let half = next.duration_since(self.this_update).unwrap_or_default() / 2;
                self.this_update + half
            }
            None => self.this_update + NO_NEXT_UPDATE_REFRESH,
        }
    }

    /// Whether it may still be served at `now`.
    pub fn valid_at(&self, now: SystemTime) -> bool {
        self.next_update.is_none_or(|next| now <= next + SKEW)
    }
}

/// Verifies `signature` over `message` with a key from a certificate.
pub fn verify(
    algorithm: &[u8],
    key_algorithm: &[u8],
    public_key: &[u8],
    message: &[u8],
    signature: &[u8],
) -> bool {
    use ring::signature as sig;
    const EC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
    const RSA_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
    const ED25519: &[u8] = &[0x2b, 0x65, 0x70];
    let choices: &[&dyn sig::VerificationAlgorithm] = match (algorithm, key_algorithm) {
        ([0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02], EC_KEY) => {
            &[&sig::ECDSA_P256_SHA256_ASN1, &sig::ECDSA_P384_SHA256_ASN1]
        }
        ([0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03], EC_KEY) => {
            &[&sig::ECDSA_P384_SHA384_ASN1, &sig::ECDSA_P256_SHA384_ASN1]
        }
        ([0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b], RSA_KEY) => {
            &[&sig::RSA_PKCS1_2048_8192_SHA256]
        }
        ([0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c], RSA_KEY) => {
            &[&sig::RSA_PKCS1_2048_8192_SHA384]
        }
        ([0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d], RSA_KEY) => {
            &[&sig::RSA_PKCS1_2048_8192_SHA512]
        }
        (ED25519, ED25519) => &[&sig::ED25519],
        _ => &[],
    };
    choices.iter().any(|alg| {
        sig::UnparsedPublicKey::new(*alg, public_key)
            .verify(message, signature)
            .is_ok()
    })
}

/// Checks an `OCSPResponse` for `id` from the CA `issuer`, as the module
/// documentation describes.
pub fn check(
    response: &[u8],
    id: &CertId,
    issuer: &Tbs<'_>,
    now: SystemTime,
) -> Result<Staple, OcspError> {
    let mut outer = Reader::new(response);
    let mut resp = outer.sequence()?;
    outer.finish()?;
    let status = resp.read(tag::ENUMERATED)?;
    if status != [0] {
        return Err(OcspError::Status(status.first().copied().unwrap_or(255)));
    }
    let mut bytes = Reader::new(resp.read(tag::context(0))?).sequence()?;
    if bytes.read(tag::OID)? != OID_OCSP_BASIC {
        return Err(OcspError::NotBasic);
    }
    let mut basic_outer = Reader::new(bytes.read(tag::OCTET_STRING)?);
    let mut basic = basic_outer.sequence()?;
    basic_outer.finish()?;
    let (t, data, signed) = basic.any()?;
    if t != tag::SEQUENCE {
        return Err(OcspError::Malformed);
    }
    let algorithm = basic.sequence()?.read(tag::OID)?;
    let signature = der::bits(basic.read(tag::BIT_STRING)?)?;
    let mut delegates = Vec::new();
    if let Some(certs) = basic.optional(tag::context(0))? {
        let mut list = Reader::new(certs).sequence()?;
        while !list.is_empty() {
            delegates.push(der::whole(&mut list, tag::SEQUENCE)?);
        }
    }

    // The signer: the issuer itself, or a delegated responder the issuer
    // signed for OCSP.
    let mut signed_ok = verify(
        algorithm,
        issuer.key_algorithm,
        issuer.public_key,
        signed,
        signature,
    );
    for cert in &delegates {
        if signed_ok {
            break;
        }
        let d = der::tbs(cert)?;
        let for_ocsp = d
            .extended_key_usage()?
            .is_some_and(|ekus| ekus.contains(&der::OID_KP_OCSP_SIGNING));
        signed_ok = for_ocsp
            && d.issuer == issuer.subject
            && verify(
                d.signature_algorithm,
                issuer.key_algorithm,
                issuer.public_key,
                d.signed,
                d.signature,
            )
            && verify(algorithm, d.key_algorithm, d.public_key, signed, signature);
    }
    if !signed_ok {
        return Err(OcspError::Signature);
    }

    let mut data = Reader::new(data);
    data.optional(tag::context(0))?; // version
    data.any()?; // responderID
    data.read(tag::GENERALIZED_TIME)?; // producedAt
    let mut responses = data.sequence()?;
    while !responses.is_empty() {
        let mut single = responses.sequence()?;
        let cert_id = single.sequence()?;
        if !id.matches(cert_id)? {
            continue;
        }
        let (status, _, _) = single.any()?;
        let this_update = der::time(&mut single)?;
        let next_update = match single.optional(tag::context(0))? {
            Some(inner) => Some(der::time(&mut Reader::new(inner))?),
            None => None,
        };
        match status {
            s if s == tag::context_primitive(0) => {}
            s if s == tag::context(1) => return Err(OcspError::Revoked),
            _ => return Err(OcspError::Unknown),
        }
        let staple = Staple {
            der: response.to_vec(),
            this_update,
            next_update,
        };
        if this_update > now + SKEW || !staple.valid_at(now) {
            return Err(OcspError::Stale);
        }
        return Ok(staple);
    }
    Err(OcspError::NoResponse)
}

/// Sends `request` to an `http://` responder and returns the body.
pub async fn fetch(url: &str, request: Vec<u8>) -> Result<Vec<u8>, OcspError> {
    let fail = |e: &dyn fmt::Display| OcspError::Fetch(format!("{url}: {e}"));
    let uri: http::Uri = url.parse().map_err(|e| fail(&e))?;
    if uri.scheme_str() != Some("http") {
        return Err(fail(&"only http:// responders are supported"));
    }
    let host = uri.host().ok_or_else(|| fail(&"no host"))?.to_owned();
    let port = uri.port_u16().unwrap_or(80);
    let path = uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
    let work = async {
        let tcp = tokio::net::TcpStream::connect((host.trim_matches(['[', ']']), port))
            .await
            .map_err(|e| fail(&e))?;
        let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
            .await
            .map_err(|e| fail(&e))?;
        tokio::spawn(conn);
        let authority = uri.authority().map_or(host.clone(), |a| a.to_string());
        let req = http::Request::post(path)
            .header(http::header::HOST, authority)
            .header(http::header::CONTENT_TYPE, "application/ocsp-request")
            .header(http::header::ACCEPT, "application/ocsp-response")
            .body(Full::new(Bytes::from(request)))
            .map_err(|e| fail(&e))?;
        let resp = send.send_request(req).await.map_err(|e| fail(&e))?;
        if resp.status() != http::StatusCode::OK {
            return Err(fail(&format!("status {}", resp.status())));
        }
        let body = Limited::new(resp.into_body(), MAX_RESPONSE_BYTES)
            .collect()
            .await
            .map_err(|e| fail(&e))?;
        Ok(body.to_bytes().to_vec())
    };
    tokio::time::timeout(FETCH_TIMEOUT, work)
        .await
        .map_err(|_| fail(&"timed out"))?
}

/// Every listener's staples, by the SHA-256 of the certificate.
#[derive(Debug, Default)]
pub struct Staples {
    map: Mutex<HashMap<[u8; 32], Staple>>,
}

impl Staples {
    pub fn key(leaf: &[u8]) -> [u8; 32] {
        Sha256::digest(leaf).into()
    }

    /// The staple for `leaf`, if one is held and still valid at `now`.
    pub fn get(&self, leaf: &[u8], now: SystemTime) -> Option<Staple> {
        let map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        map.get(&Staples::key(leaf))
            .filter(|s| s.valid_at(now))
            .cloned()
    }

    fn put(&self, leaf: &[u8], staple: Option<Staple>) {
        let mut map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        match staple {
            Some(s) => map.insert(Staples::key(leaf), s),
            None => map.remove(&Staples::key(leaf)),
        };
    }
}

/// Wall-clock time that moves with a [`Clock`], so a manual clock moves
/// the wall time the stapler compares OCSP dates with.
#[derive(Debug, Clone)]
pub struct Wall {
    clock: Arc<dyn Clock>,
    origin: Instant,
    at: SystemTime,
}

impl Wall {
    /// The wall time is `at` when `clock` reads what it reads now.
    pub fn new(clock: Arc<dyn Clock>, at: SystemTime) -> Wall {
        let origin = clock.now();
        Wall { clock, origin, at }
    }

    /// The system's wall clock over the monotonic clock.
    pub fn system() -> Wall {
        Wall::new(crate::clock::MonotonicClock::shared(), SystemTime::now())
    }

    pub fn now(&self) -> SystemTime {
        self.at + self.clock.now().saturating_duration_since(self.origin)
    }
}

/// What the stapler tracks per certificate.
struct Tracked {
    due: SystemTime,
    failures: u32,
}

/// Fetches and refreshes the staples of every certificate in a store's
/// current snapshot; see the module documentation.
pub struct Stapler {
    store: Arc<Store>,
    staples: Arc<Staples>,
    wall: Wall,
    /// Replaces each certificate's OCSP URL.
    responder: Option<String>,
    tracked: HashMap<[u8; 32], Tracked>,
    rng: u64,
}

impl Stapler {
    pub fn new(
        store: Arc<Store>,
        staples: Arc<Staples>,
        wall: Wall,
        responder: Option<String>,
    ) -> Stapler {
        let seed = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64)
            | 1;
        Stapler {
            store,
            staples,
            wall,
            responder,
            tracked: HashMap::new(),
            rng: seed,
        }
    }

    fn jitter(&mut self, d: Duration) -> Duration {
        // xorshift64*: enough to spread retries.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let r = self.rng.wrapping_mul(0x2545_f491_4f6c_dd1d);
        d + d.mul_f64((r >> 11) as f64 / (1u64 << 53) as f64 * 0.1)
    }

    /// Brings every certificate of the current snapshot up to date:
    /// fetches what is due, withdraws what has expired, and returns how
    /// long until the next one is due.
    pub async fn tick(&mut self) -> Duration {
        let snapshot = self.store.current();
        let now = self.wall.now();
        let mut next: Option<SystemTime> = None;
        for i in 0..snapshot.certificates().len() {
            let chain = snapshot.chain(i);
            let Some(leaf) = chain.first() else { continue };
            let key = Staples::key(leaf);
            let Ok(leaf_tbs) = der::tbs(leaf) else {
                continue;
            };
            let Some(issuer) = chain.get(1) else { continue };
            let Ok(issuer_tbs) = der::tbs(issuer) else {
                continue;
            };
            let url = match &self.responder {
                Some(r) => r.clone(),
                None => match leaf_tbs.ocsp_urls().ok().and_then(|u| u.into_iter().next()) {
                    Some(u) => u,
                    None => continue,
                },
            };
            let due = self.tracked.get(&key).map_or(now, |t| t.due);
            if now >= due {
                let id = CertId::new(&leaf_tbs, &issuer_tbs);
                let result = match fetch(&url, id.request()).await {
                    Ok(body) => check(&body, &id, &issuer_tbs, self.wall.now()),
                    Err(e) => Err(e),
                };
                let now = self.wall.now();
                let (staple, due) = match result {
                    Ok(staple) => {
                        tracing::debug!(
                            cert = snapshot.certificates()[i].cert_file,
                            "ocsp staple fetched"
                        );
                        let due = staple.refresh_at().max(now + Duration::from_secs(1));
                        self.tracked.insert(key, Tracked { due, failures: 0 });
                        (Some(staple), due)
                    }
                    Err(e) => {
                        let failures = self.tracked.get(&key).map_or(0, |t| t.failures) + 1;
                        let backoff = FIRST_RETRY
                            .saturating_mul(1 << (failures - 1).min(10))
                            .min(MAX_RETRY);
                        let due = now + self.jitter(backoff);
                        tracing::warn!(
                            cert = snapshot.certificates()[i].cert_file,
                            error = %e,
                            retry_in_ms = due.duration_since(now).unwrap_or_default().as_millis() as u64,
                            "ocsp staple not refreshed"
                        );
                        self.tracked.insert(key, Tracked { due, failures });
                        // Keep a staple that is still valid.
                        (self.staples.get(leaf, now), due)
                    }
                };
                self.staples.put(leaf, staple.clone());
                snapshot.set_staple(i, staple.map(|s| s.der));
                next = Some(next.map_or(due, |n| n.min(due)));
            } else {
                // Serve what is held, and withdraw a staple whose
                // validity ended while waiting.
                let held = self.staples.get(leaf, now);
                if let Some(s) = &held
                    && let Some(n) = s.next_update
                {
                    let expiry = n + SKEW;
                    next = Some(next.map_or(expiry, |x| x.min(expiry)));
                }
                snapshot.set_staple(i, held.map(|s| s.der));
                next = Some(next.map_or(due, |n| n.min(due)));
            }
        }
        let now = self.wall.now();
        next.map_or(MAX_RETRY, |n| n.duration_since(now).unwrap_or_default())
            .clamp(Duration::from_secs(1), MAX_RETRY)
    }

    /// Ticks until `stop`, waking for whatever is due next or when the
    /// store is swapped.
    pub async fn run(mut self, clock: Arc<dyn Clock>, stop: impl Future<Output = ()>) {
        let mut stop = std::pin::pin!(stop);
        loop {
            let store = Arc::clone(&self.store);
            let mut swapped = std::pin::pin!(store.swapped());
            swapped.as_mut().enable();
            let wait = self.tick().await;
            tokio::select! {
                () = &mut stop => return,
                _ = clock.after(wait) => {}
                () = swapped => {}
            }
        }
    }
}
