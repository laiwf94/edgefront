//! steer's `internal/tlsedge` on rustls: certificates chosen by SNI, and
//! handshakes run off the accept loop with a timeout and a cap on how
//! many run at once.
//!
//! A [`Snapshot`] is one loaded `tls` block: every certificate and key,
//! read and checked, the SNI table built from them, and a rustls
//! [`ServerConfig`] with the block's minimum version and suites whose
//! certificate resolver is that table. It is immutable.
//!
//! Certificates are chosen by SNI as steer chose them. The client's name
//! is lower-cased and loses one trailing dot. An exact name beats a
//! wildcard, and `*.example.com` covers exactly one label
//! (`a.example.com`, not `a.b.example.com` or `example.com`), as RFC 6125
//! requires of clients. When several certificates carry the same name (an
//! ECDSA and an RSA one, say) the first the client can use wins: one whose
//! key can sign with a scheme the client offers, and, for a client that
//! offers no TLS 1.3 suite, one an offered TLS 1.2 suite can authenticate.
//! If the client can use none of them the first is taken, and the
//! handshake then fails on its own terms. A handshake that matches nothing
//! gets the first configured certificate, or is refused under
//! `strict_sni`. A certificate's names are its `server_names` when they
//! are set, otherwise the DNS names in its subject alternative names.
//!
//! The [`Acceptor`] runs handshakes. The listener takes a handshake slot
//! before it accepts, so once `max_concurrent_handshakes` are running new
//! connections wait in the kernel's backlog rather than in memory, and a
//! client that connects and stalls holds only its own slot. Each handshake
//! has the snapshot's `handshake_timeout` as a hard deadline. Every
//! handshake that ends before shutdown is reported with its
//! [`HandshakeResult`] and duration.
//!
//! Two things differ from steer, both because rustls does not do them:
//! the four AES-CBC suites of the `compatible` policy are not offered (a
//! snapshot lists the ones it dropped in [`Snapshot::warnings`], and a
//! `cipher_suites` list of only CBC suites is refused), and a client
//! whose ALPN list has no `http/1.1` is refused with
//! `no_application_protocol` where Go would carry on without ALPN.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};

use rustls::crypto::{CryptoProvider, ring};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::{ClientHello, ParsedCertificate, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{ServerConfig, SignatureAlgorithm, SupportedCipherSuite};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_rustls::server::TlsStream;

use super::client_auth::{self, ClientIdentity};
use super::der;
use super::ocsp::Staples;
use super::store::Store;
use super::tickets::TicketKeys;
use crate::acme::challenges::{ACME_TLS_ALPN, AlpnChallenges};
use crate::config::{CipherSuite, Tls, TlsVersion};
pub use crate::telemetry::metrics::HandshakeResult;

/// The largest certificate or key file read, 1 MiB. A chain is a few KiB.
pub const MAX_PEM_BYTES: u64 = 1 << 20;

/// The ALPN protocol the listener offers.
pub const ALPN_HTTP11: &[u8] = b"http/1.1";

/// One loaded certificate, as [`Snapshot::certificates`] describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    pub cert_file: String,
    /// The names it is chosen for, normalized and without duplicates.
    pub names: Vec<String>,
    pub not_before: SystemTime,
    pub not_after: SystemTime,
}

/// Why a `tls` block could not be loaded.
#[derive(Debug)]
pub enum LoadError {
    /// A certificate or key could not be read, parsed or matched; the
    /// message names the entry and the file, as steer's did.
    Certificate(String),
    /// The block names suites, but none rustls can offer.
    NoSuites,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Certificate(m) => f.write_str(m),
            LoadError::NoSuites => f.write_str(
                "tls.cipher_suites: none of the listed suites is available; AES-CBC suites are not supported",
            ),
        }
    }
}

impl std::error::Error for LoadError {}

/// The certificate the SNI table chose, or the refusal under strict SNI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownServerName;

impl fmt::Display for UnknownServerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("no certificate for the requested server name")
    }
}

impl std::error::Error for UnknownServerName {}

/// The SNI table: certificate indexes by exact name and by wildcard
/// suffix, in configuration order.
#[derive(Debug)]
struct Table {
    /// Each certificate and key, replaced whole when its OCSP staple
    /// changes.
    keys: Vec<RwLock<Arc<CertifiedKey>>>,
    exact: HashMap<String, Vec<usize>>,
    /// Keyed by the suffix after `*.`.
    wildcard: HashMap<String, Vec<usize>>,
    strict: bool,
}

impl Table {
    fn key(&self, i: usize) -> Arc<CertifiedKey> {
        Arc::clone(&self.keys[i].read().unwrap_or_else(|p| p.into_inner()))
    }

    /// The index of the certificate for `name`, with `usable` saying
    /// which certificates the client can use.
    fn select(
        &self,
        name: Option<&str>,
        usable: impl Fn(&CertifiedKey) -> bool,
    ) -> Result<usize, UnknownServerName> {
        if self.keys.is_empty() {
            // Only an ACME listener waiting for its first certificate.
            return Err(UnknownServerName);
        }
        let name = normalize(name.unwrap_or_default());
        if !name.is_empty() {
            if let Some(i) = self.pick(self.exact.get(name.as_str()), &usable) {
                return Ok(i);
            }
            if let Some((label, suffix)) = name.split_once('.')
                && !label.is_empty()
                && !suffix.is_empty()
                && let Some(i) = self.pick(self.wildcard.get(suffix), &usable)
            {
                return Ok(i);
            }
        }
        if self.strict {
            Err(UnknownServerName)
        } else {
            Ok(0)
        }
    }

    fn pick(
        &self,
        candidates: Option<&Vec<usize>>,
        usable: &impl Fn(&CertifiedKey) -> bool,
    ) -> Option<usize> {
        let candidates = candidates?;
        if candidates.len() > 1 {
            for &i in candidates {
                if usable(&self.key(i)) {
                    return Some(i);
                }
            }
        }
        candidates.first().copied()
    }
}

/// Whether a client whose hello is `hello` can use `key`, as Go's
/// `SupportsCertificate` decided for the cases rustls can negotiate.
fn usable(key: &CertifiedKey, hello: &ClientHello<'_>) -> bool {
    if key.key.choose_scheme(hello.signature_schemes()).is_none() {
        return false;
    }
    let suites = hello.cipher_suites();
    if suites
        .iter()
        .any(|s| matches!(u16::from(*s), 0x1301..=0x1303))
    {
        return true;
    }
    let alg = key.key.algorithm();
    suites
        .iter()
        .any(|s| tls12_auth(u16::from(*s)) == Some(alg))
}

/// The key type a TLS 1.2 ECDHE suite authenticates with.
fn tls12_auth(id: u16) -> Option<SignatureAlgorithm> {
    match id {
        0xc009 | 0xc00a | 0xc023 | 0xc024 | 0xc02b | 0xc02c | 0xcca9 => {
            Some(SignatureAlgorithm::ECDSA)
        }
        0xc013 | 0xc014 | 0xc027 | 0xc028 | 0xc02f | 0xc030 | 0xcca8 => {
            Some(SignatureAlgorithm::RSA)
        }
        _ => None,
    }
}

#[derive(Debug)]
struct Resolver(Arc<Table>, Option<Arc<AlpnChallenges>>);

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        // A TLS-ALPN-01 validation (RFC 8737) gets the challenge
        // certificate for its name, or nothing; never a real certificate.
        if let Some(challenges) = &self.1
            && hello
                .alpn()
                .is_some_and(|mut protos| protos.any(|p| p == ACME_TLS_ALPN))
        {
            return challenges.get(hello.server_name()?);
        }
        let i = self
            .0
            .select(hello.server_name(), |k| usable(k, &hello))
            .ok()?;
        Some(self.0.key(i))
    }
}

/// What a listener's snapshots share across reloads.
#[derive(Debug, Clone, Default)]
pub struct Shared {
    /// Session ticket keys; `None` leaves rustls's default (TLS 1.3
    /// resumption from a per-snapshot session cache, which a reload
    /// empties, and no TLS 1.2 tickets).
    pub tickets: Option<Arc<TicketKeys>>,
    /// OCSP staples already fetched, applied to a new snapshot's
    /// certificates as it loads.
    pub staples: Option<Arc<Staples>>,
    /// TLS-ALPN-01 challenge certificates, served to `acme-tls/1`
    /// handshakes; `None` does not offer that protocol at all.
    pub alpn_challenges: Option<Arc<AlpnChallenges>>,
}

/// One loaded `tls` block. Immutable; a reload builds a new one.
#[derive(Debug)]
pub struct Snapshot {
    config: Arc<ServerConfig>,
    table: Arc<Table>,
    certs: Vec<CertInfo>,
    /// Configured suites rustls does not implement.
    dropped_suites: Vec<&'static str>,
    pub handshake_timeout: Duration,
    pub max_concurrent_handshakes: usize,
    /// A client certificate is required but checked after the handshake.
    require_client_after_handshake: bool,
}

impl Snapshot {
    /// Reads every certificate and key in `tls` and builds a snapshot. It
    /// fails on the first file that cannot be read or parsed, whose key
    /// does not match its certificate, or (past the first, which is the
    /// fallback) whose certificate has no names to be chosen by.
    pub fn load(tls: &Tls) -> Result<Snapshot, LoadError> {
        Snapshot::load_with(tls, &Shared::default())
    }

    /// Like [`load`](Self::load), with what outlives a reload: the
    /// session ticket keys every snapshot of a listener shares, so tickets
    /// issued before a reload still resume after it.
    pub fn load_with(tls: &Tls, shared: &Shared) -> Result<Snapshot, LoadError> {
        let mut table = Table {
            keys: Vec::new(),
            exact: HashMap::new(),
            wildcard: HashMap::new(),
            strict: tls.strict_sni,
        };
        let mut certs = Vec::new();
        for (i, c) in tls.certificates.iter().enumerate() {
            let fail = |m: String| LoadError::Certificate(format!("tls.certificates[{i}]: {m}"));
            let (key, fields) = load_pair(&c.cert_file, &c.key_file).map_err(fail)?;
            let names = match &c.server_names {
                Some(names) => dedupe(names.clone()),
                None => dedupe(fields.dns_names.iter().map(|n| normalize(n)).collect()),
            };
            if names.is_empty() && i > 0 {
                return Err(fail(format!(
                    "{} has no DNS names; set server_names",
                    c.cert_file
                )));
            }
            let index = table.keys.len();
            for n in &names {
                match n.strip_prefix("*.") {
                    Some(suffix) => table.wildcard.entry(suffix.to_owned()),
                    None => table.exact.entry(n.clone()),
                }
                .or_default()
                .push(index);
            }
            let mut key = key;
            if let Some(staples) = &shared.staples
                && let Some(leaf) = key.cert.first()
            {
                key.ocsp = staples.get(leaf, SystemTime::now()).map(|s| s.der);
            }
            table.keys.push(RwLock::new(Arc::new(key)));
            certs.push(CertInfo {
                cert_file: c.cert_file.clone(),
                names,
                not_before: fields.not_before,
                not_after: fields.not_after,
            });
        }
        if table.keys.is_empty() && tls.acme.is_none() {
            return Err(LoadError::Certificate(
                "tls.certificates: at least one certificate is required".to_owned(),
            ));
        }
        let (suites, dropped_suites) = suites(tls.cipher_suites.as_deref());
        if tls.cipher_suites.as_ref().is_some_and(|s| !s.is_empty())
            && !suites.iter().any(|s| s.tls13().is_none())
        {
            return Err(LoadError::NoSuites);
        }
        let table = Arc::new(table);
        let provider = CryptoProvider {
            cipher_suites: suites,
            ..ring::default_provider()
        };
        let versions: &[&rustls::SupportedProtocolVersion] = match tls.min_version {
            TlsVersion::Tls12 => &[&rustls::version::TLS13, &rustls::version::TLS12],
            TlsVersion::Tls13 => &[&rustls::version::TLS13],
        };
        // With TLS-ALPN-01 on, a required client certificate is checked
        // after the handshake instead of in it, so the CA's validation
        // handshake (which has none) completes.
        let defer_client_auth = shared.alpn_challenges.is_some()
            && tls.client_auth.as_ref().is_some_and(|a| a.required);
        let mut config = ServerConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(versions)
            .expect("ring supports TLS 1.2 and 1.3 with these suites")
            .with_client_cert_verifier(match &tls.client_auth {
                None => Arc::new(rustls::server::NoClientAuth),
                Some(auth) => client_auth::verifier(auth, defer_client_auth)
                    .map_err(LoadError::Certificate)?,
            })
            .with_cert_resolver(Arc::new(Resolver(
                Arc::clone(&table),
                shared.alpn_challenges.clone(),
            )));
        config.alpn_protocols = vec![ALPN_HTTP11.to_vec()];
        if shared.alpn_challenges.is_some() {
            config.alpn_protocols.push(ACME_TLS_ALPN.to_vec());
        }
        if let Some(keys) = &shared.tickets {
            config.ticketer = Arc::clone(keys) as Arc<dyn rustls::server::ProducesTickets>;
        }
        Ok(Snapshot {
            config: Arc::new(config),
            table,
            certs,
            dropped_suites,
            handshake_timeout: tls.handshake_timeout,
            require_client_after_handshake: defer_client_auth,
            max_concurrent_handshakes: usize::try_from(tls.max_concurrent_handshakes)
                .unwrap_or(usize::MAX)
                .max(1),
        })
    }

    /// The rustls configuration handshakes on this snapshot use.
    pub fn server_config(&self) -> &Arc<ServerConfig> {
        &self.config
    }

    /// The loaded certificates in configuration order.
    pub fn certificates(&self) -> &[CertInfo] {
        &self.certs
    }

    /// The chain of certificate `i`, leaf first, as served.
    pub fn chain(&self, i: usize) -> Vec<CertificateDer<'static>> {
        self.table.key(i).cert.clone()
    }

    /// The OCSP response stapled to certificate `i`.
    pub fn staple(&self, i: usize) -> Option<Vec<u8>> {
        self.table.key(i).ocsp.clone()
    }

    /// Staples `ocsp` to certificate `i` (or withdraws its staple) for
    /// every handshake from now on.
    pub fn set_staple(&self, i: usize, ocsp: Option<Vec<u8>>) {
        let mut slot = self.table.keys[i]
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if slot.ocsp != ocsp {
            let mut next = CertifiedKey::clone(&slot);
            next.ocsp = ocsp;
            *slot = Arc::new(next);
        }
    }

    /// The index in [`certificates`](Self::certificates) of the
    /// certificate a handshake for `server_name` gets from a client that
    /// can use any of them.
    pub fn select(&self, server_name: &str) -> Result<usize, UnknownServerName> {
        self.table.select(Some(server_name), |_| true)
    }

    /// Problems that do not stop the snapshot from serving: certificates
    /// outside their validity period at `now`, names a certificate does
    /// not cover, and configured suites that are not offered.
    pub fn warnings(&self, now: SystemTime) -> Vec<String> {
        let mut out = Vec::new();
        for c in &self.certs {
            if now > c.not_after {
                out.push(format!(
                    "{} expired at {}",
                    c.cert_file,
                    rfc3339(c.not_after)
                ));
            } else if now < c.not_before {
                out.push(format!(
                    "{} is not valid until {}",
                    c.cert_file,
                    rfc3339(c.not_before)
                ));
            }
        }
        for (i, c) in self.certs.iter().enumerate() {
            let key = self.table.key(i);
            let Some(leaf) = key.cert.first() else {
                continue;
            };
            let Ok(parsed) = ParsedCertificate::try_from(leaf) else {
                continue;
            };
            for n in &c.names {
                let host = match n.strip_prefix("*.") {
                    Some(suffix) => format!("steer-wildcard-probe.{suffix}"),
                    None => n.clone(),
                };
                let covered = ServerName::try_from(host.as_str())
                    .is_ok_and(|sn| rustls::client::verify_server_name(&parsed, &sn).is_ok());
                if !covered {
                    out.push(format!(
                        "{} does not cover server name {}",
                        c.cert_file,
                        crate::config::go::quote(n)
                    ));
                }
            }
        }
        for s in &self.dropped_suites {
            out.push(format!(
                "cipher suite {s} is not offered: AES-CBC suites are not supported"
            ));
        }
        out
    }
}

/// The rustls suites for a configured list: every TLS 1.3 suite, then the
/// listed TLS 1.2 suites in order. Returns the suites and the names of
/// listed ones ring does not implement.
fn suites(listed: Option<&[CipherSuite]>) -> (Vec<SupportedCipherSuite>, Vec<&'static str>) {
    let mut out = vec![
        ring::cipher_suite::TLS13_AES_128_GCM_SHA256,
        ring::cipher_suite::TLS13_AES_256_GCM_SHA384,
        ring::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
    ];
    let mut dropped = Vec::new();
    for s in listed.unwrap_or_default() {
        let found = ring::ALL_CIPHER_SUITES
            .iter()
            .find(|r| u16::from(r.suite()) == s.id && r.tls13().is_none());
        match found {
            Some(r) => out.push(*r),
            None => dropped.push(s.name),
        }
    }
    (out, dropped)
}

/// Reads a PEM file of at most [`MAX_PEM_BYTES`].
fn read_pem(path: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let open = |e: io::Error| format!("open {path}: {e}");
    let file = std::fs::File::open(path).map_err(open)?;
    let mut data = Vec::new();
    file.take(MAX_PEM_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(open)?;
    if data.len() as u64 > MAX_PEM_BYTES {
        return Err(format!("{path}: larger than {MAX_PEM_BYTES} bytes"));
    }
    Ok(data)
}

/// Loads a certificate chain and its key, as Go's `LoadX509KeyPair` did,
/// and reads the leaf's fields.
pub(crate) fn load_pair(
    cert_file: &str,
    key_file: &str,
) -> Result<(CertifiedKey, der::CertFields), String> {
    let pem = read_pem(cert_file)?;
    let chain = CertificateDer::pem_slice_iter(&pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{cert_file}: {e}"))?;
    let Some(leaf) = chain.first() else {
        return Err(format!(
            "{cert_file}: failed to find any PEM data in certificate input"
        ));
    };
    let fields = der::cert_fields(leaf).map_err(|e| format!("{cert_file}: {e}"))?;
    let pem = read_pem(key_file)?;
    let key = PrivateKeyDer::from_pem_slice(&pem).map_err(|e| match e {
        rustls::pki_types::pem::Error::NoItemsFound => {
            format!("{key_file}: failed to find any PEM data in key input")
        }
        e => format!("{key_file}: {e}"),
    })?;
    let key = CertifiedKey::from_der(chain, key, &ring::default_provider())
        .map_err(|e| format!("{cert_file} and {key_file}: {e}"))?;
    Ok((key, fields))
}

/// Lower-cases a server name and drops one trailing dot.
fn normalize(n: &str) -> String {
    let n = n.to_ascii_lowercase();
    match n.strip_suffix('.') {
        Some(s) => s.to_owned(),
        None => n,
    }
}

fn dedupe(names: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for n in names {
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// A time as RFC 3339 in UTC, as Go's `time.RFC3339` printed it.
fn rfc3339(t: SystemTime) -> String {
    let secs = match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let (year, month, day) = der::civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Called after every handshake that ends before shutdown.
pub type OnHandshake = Arc<dyn Fn(HandshakeResult, Duration) + Send + Sync>;

/// What a request on a TLS connection can read about it, from the
/// request's extensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsInfo {
    /// The SNI the client sent, as sent.
    pub server_name: Option<String>,
    /// The negotiated ALPN protocol.
    pub alpn: Option<Vec<u8>>,
    /// The verified client certificate's identity, under `client_auth`.
    pub client: Option<ClientIdentity>,
    /// The negotiated protocol version, such as `0x0304` for TLS 1.3.
    pub version: u16,
    /// The negotiated cipher suite's IANA number.
    pub cipher_suite: u16,
}

impl TlsInfo {
    fn of(stream: &TlsStream<TcpStream>) -> TlsInfo {
        let (_, conn) = stream.get_ref();
        TlsInfo {
            server_name: conn.server_name().map(str::to_owned),
            alpn: conn.alpn_protocol().map(<[u8]>::to_vec),
            client: conn
                .peer_certificates()
                .and_then(|c| c.first())
                .map(ClientIdentity::of),
            version: conn.protocol_version().map_or(0, u16::from),
            cipher_suite: conn
                .negotiated_cipher_suite()
                .map_or(0, |s| u16::from(s.suite())),
        }
    }
}

/// Whether a finished handshake lacks the client certificate a deferred
/// `client_auth` requires; an `acme-tls/1` validation never needs one.
fn missing_client(s: &TlsStream<TcpStream>) -> bool {
    let conn = s.get_ref().1;
    conn.peer_certificates().is_none_or(<[_]>::is_empty)
        && conn.alpn_protocol() != Some(ACME_TLS_ALPN)
}

/// Runs TLS handshakes for a listener, each on the store's snapshot at
/// the moment it starts.
pub struct Acceptor {
    store: Arc<Store>,
    /// Slots taken: handshakes running, plus the one the accept loop
    /// holds while it waits for a connection.
    taken: Mutex<usize>,
    freed: Notify,
    active: AtomicUsize,
    on_handshake: OnceLock<OnHandshake>,
}

impl fmt::Debug for Acceptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Acceptor")
            .field("in_progress", &self.in_progress())
            .finish_non_exhaustive()
    }
}

/// A handshake slot, given back when dropped.
pub struct Slot(Arc<Acceptor>);

impl Drop for Slot {
    fn drop(&mut self) {
        let mut taken = self.0.taken.lock().unwrap_or_else(|p| p.into_inner());
        *taken -= 1;
        drop(taken);
        self.0.freed.notify_one();
    }
}

impl Acceptor {
    /// An acceptor serving `store`'s snapshots. `on_handshake`, if set,
    /// is called after every handshake that ends before shutdown.
    pub fn new(store: Arc<Store>, on_handshake: Option<OnHandshake>) -> Arc<Acceptor> {
        Arc::new(Acceptor {
            store,
            taken: Mutex::new(0),
            freed: Notify::new(),
            active: AtomicUsize::new(0),
            on_handshake: on_handshake.map(OnceLock::from).unwrap_or_default(),
        })
    }

    /// Sets the handshake callback after construction, for a callback
    /// that needs the acceptor (a gauge reading
    /// [`in_progress`](Self::in_progress), say). Returns false, leaving
    /// the callback as it was, if one is already set.
    pub fn set_on_handshake(&self, on_handshake: OnHandshake) -> bool {
        self.on_handshake.set(on_handshake).is_ok()
    }

    /// The store the acceptor reads.
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// The snapshot new handshakes use.
    pub fn current(&self) -> Arc<Snapshot> {
        self.store.current()
    }

    /// Handshakes running now.
    pub fn in_progress(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// Waits for a handshake slot: until fewer than the current
    /// snapshot's `max_concurrent_handshakes` are taken.
    pub async fn slot(self: &Arc<Self>) -> Slot {
        loop {
            {
                let mut taken = self.taken.lock().unwrap_or_else(|p| p.into_inner());
                if *taken < self.current().max_concurrent_handshakes {
                    *taken += 1;
                    return Slot(Arc::clone(self));
                }
            }
            // notify_one keeps a permit when nobody waits, so a slot freed
            // between the check and this wait is not missed.
            self.freed.notified().await;
        }
    }

    /// Runs the server side of a handshake on `stream` under `slot`,
    /// within the current snapshot's timeout. Returns the established
    /// stream and what a request on it can read, or `None` when the
    /// handshake failed; either way the result is reported.
    pub async fn handshake(
        &self,
        slot: Slot,
        stream: TcpStream,
    ) -> Option<(TlsStream<TcpStream>, Arc<TlsInfo>)> {
        /// Counts a handshake in progress until dropped, so one abandoned
        /// at shutdown is not left counted.
        struct Active<'a>(&'a AtomicUsize);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let snapshot = self.current();
        self.active.fetch_add(1, Ordering::Relaxed);
        let active = Active(&self.active);
        let start = Instant::now();
        let accept = tokio_rustls::TlsAcceptor::from(Arc::clone(&snapshot.config)).accept(stream);
        let outcome = tokio::time::timeout(snapshot.handshake_timeout, accept).await;
        drop(active);
        drop(slot);
        let (result, stream) = match outcome {
            Ok(Ok(s)) if snapshot.require_client_after_handshake && missing_client(&s) => {
                tracing::debug!("tls client sent no certificate; one is required");
                (HandshakeResult::Error, None)
            }
            Ok(Ok(s)) => (HandshakeResult::Ok, Some(s)),
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "tls handshake failed");
                (HandshakeResult::Error, None)
            }
            Err(_) => (HandshakeResult::Timeout, None),
        };
        if let Some(f) = self.on_handshake.get() {
            f(result, start.elapsed());
        }
        stream.map(|s| {
            let info = Arc::new(TlsInfo::of(&s));
            (s, info)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_as_go() {
        let at = |s: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(s);
        assert_eq!(rfc3339(at(0)), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(at(1_709_208_000)), "2024-02-29T12:00:00Z");
        assert_eq!(rfc3339(at(4_922_899_200)), "2126-01-01T00:00:00Z");
        assert_eq!(
            rfc3339(SystemTime::UNIX_EPOCH - Duration::from_secs(1)),
            "1969-12-31T23:59:59Z"
        );
    }

    #[test]
    fn names_normalize_as_steer() {
        assert_eq!(normalize("API.Example.COM."), "api.example.com");
        assert_eq!(normalize("a.."), "a.");
        assert_eq!(
            dedupe(vec!["a".into(), "b".into(), "a".into()]),
            vec!["a".to_owned(), "b".to_owned()]
        );
    }

    #[test]
    fn tls12_suites_by_key_type() {
        assert_eq!(tls12_auth(0xc02b), Some(SignatureAlgorithm::ECDSA));
        assert_eq!(tls12_auth(0xc02f), Some(SignatureAlgorithm::RSA));
        assert_eq!(tls12_auth(0x1301), None);
        assert_eq!(
            UnknownServerName.to_string(),
            "no certificate for the requested server name"
        );
        assert!(LoadError::NoSuites.to_string().contains("AES-CBC"));
    }
}
