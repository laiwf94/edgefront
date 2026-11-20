//! OCSP stapling: responses checked as the module documentation says
//! (signer, status, certificate, validity), and the stapler fetching,
//! refreshing, keeping and withdrawing staples on a manual clock, with
//! clients reading the staple from real handshakes.

#[path = "common/tls.rs"]
mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use common::*;
use edgefront_core::clock::{Clock, ManualClock};
use edgefront_core::listener::{Handler, ServerLimits, serve_tls};
use edgefront_core::tls::der::{self, write};
use edgefront_core::tls::ocsp::{self, CertId, OcspError, Stapler, Staples, Wall};
use edgefront_core::tls::{Acceptor, Shared, Snapshot, Store};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use tokio::net::{TcpListener, TcpStream};

const ECDSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
const OCSP_BASIC: &[u8] = &[0x2b, 6, 1, 5, 5, 7, 48, 1, 1];

fn signer(base: &str) -> EcdsaKeyPair {
    let key = PrivateKeyDer::from_pem_file(fixture(&format!("{base}.key"))).unwrap();
    EcdsaKeyPair::from_pkcs8(
        &ECDSA_P256_SHA256_ASN1_SIGNING,
        key.secret_der(),
        &ring::rand::SystemRandom::new(),
    )
    .unwrap()
}

fn chain(file: &str) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_file_iter(fixture(file))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// The `CertID` of the `ocsp` fixture and its issuer.
fn ocsp_id() -> (CertId, Vec<u8>) {
    let c = chain("ocsp.crt");
    let issuer = c[1].to_vec();
    (
        CertId::new(&der::tbs(&c[0]).unwrap(), &der::tbs(&issuer).unwrap()),
        issuer,
    )
}

#[derive(Clone, Copy)]
enum Status {
    Good,
    Revoked,
    Unknown,
}

/// An `OCSPResponse` for `id`, signed by `key`, carrying `certs`.
fn response(
    id: &CertId,
    status: Status,
    this: SystemTime,
    next: Option<SystemTime>,
    key: &EcdsaKeyPair,
    certs: &[&[u8]],
) -> Vec<u8> {
    let status = match status {
        Status::Good => write::implicit(0, &[]),
        Status::Revoked => write::explicit(1, &write::generalized_time(this)),
        Status::Unknown => write::implicit(2, &[]),
    };
    let mut single = vec![id.encode(), status, write::generalized_time(this)];
    if let Some(n) = next {
        single.push(write::explicit(0, &write::generalized_time(n)));
    }
    let single: Vec<&[u8]> = single.iter().map(Vec::as_slice).collect();
    let tbs = write::sequence(&[
        &write::explicit(2, &write::octet_string(&[7; 20])),
        &write::generalized_time(this),
        &write::sequence(&[&write::sequence(&single)]),
    ]);
    let sig = key
        .sign(&ring::rand::SystemRandom::new(), &tbs)
        .unwrap()
        .as_ref()
        .to_vec();
    let mut basic = vec![
        tbs,
        write::sequence(&[&write::oid(ECDSA_SHA256)]),
        write::bit_string(&sig),
    ];
    if !certs.is_empty() {
        basic.push(write::explicit(0, &write::sequence(certs)));
    }
    let basic: Vec<&[u8]> = basic.iter().map(Vec::as_slice).collect();
    let basic = write::sequence(&basic);
    write::sequence(&[
        &write::enumerated(0),
        &write::explicit(
            0,
            &write::sequence(&[&write::oid(OCSP_BASIC), &write::octet_string(&basic)]),
        ),
    ])
}

fn at(s: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(s)
}

const NOW: u64 = 1_800_000_000;

#[test]
fn the_request_names_the_certificate() {
    let (id, _) = ocsp_id();
    assert_eq!(id.issuer_name_hash.len(), 20);
    assert_eq!(id.issuer_key_hash.len(), 20);
    let req = id.request();
    // OCSPRequest { tbsRequest { requestList { Request { reqCert } } } }
    let mut r = der::Reader::new(&req);
    let mut ocsp_request = r.sequence().unwrap();
    let mut tbs = ocsp_request.sequence().unwrap();
    let mut list = tbs.sequence().unwrap();
    let mut request = list.sequence().unwrap();
    let cert_id = der::whole(&mut request, der::tag::SEQUENCE).unwrap();
    assert_eq!(cert_id, id.encode().as_slice());
    assert!(r.finish().is_ok());
}

#[test]
fn responses_are_checked() {
    let (id, issuer) = ocsp_id();
    let issuer = der::tbs(&issuer).unwrap();
    let ca = signer("ca");
    let now = at(NOW);
    let good = |this: u64, next: Option<u64>| {
        response(&id, Status::Good, at(this), next.map(at), &ca, &[])
    };
    let s = ocsp::check(&good(NOW - 60, Some(NOW + 3600)), &id, &issuer, now).unwrap();
    assert_eq!(s.this_update, at(NOW - 60));
    assert_eq!(s.next_update, Some(at(NOW + 3600)));
    assert_eq!(s.refresh_at(), at(NOW - 60 + 1830));
    // Without nextUpdate the staple is refreshed after an hour.
    let s = ocsp::check(&good(NOW, None), &id, &issuer, now).unwrap();
    assert_eq!(s.refresh_at(), at(NOW + 3600));
    // Five minutes of slack either way, and no more.
    assert!(ocsp::check(&good(NOW + 299, Some(NOW + 9000)), &id, &issuer, now).is_ok());
    assert_eq!(
        ocsp::check(&good(NOW + 301, Some(NOW + 9000)), &id, &issuer, now),
        Err(OcspError::Stale)
    );
    assert!(ocsp::check(&good(NOW - 9000, Some(NOW - 299)), &id, &issuer, now).is_ok());
    assert_eq!(
        ocsp::check(&good(NOW - 9000, Some(NOW - 301)), &id, &issuer, now),
        Err(OcspError::Stale)
    );
    for (status, want) in [
        (Status::Revoked, OcspError::Revoked),
        (Status::Unknown, OcspError::Unknown),
    ] {
        let r = response(&id, status, at(NOW), Some(at(NOW + 60)), &ca, &[]);
        assert_eq!(ocsp::check(&r, &id, &issuer, now), Err(want));
    }
    // A response for another certificate.
    let mut other = id.clone();
    other.serial = vec![0x7f];
    let r = response(&other, Status::Good, at(NOW), None, &ca, &[]);
    assert_eq!(
        ocsp::check(&r, &id, &issuer, now),
        Err(OcspError::NoResponse)
    );
    // Signed by another CA's key.
    let r = response(&id, Status::Good, at(NOW), None, &signer("other-ca"), &[]);
    assert_eq!(
        ocsp::check(&r, &id, &issuer, now),
        Err(OcspError::Signature)
    );
    // A delegated responder the CA issued for OCSP signing.
    let delegate = chain("ocsp-responder.crt")[0].to_vec();
    let r = response(
        &id,
        Status::Good,
        at(NOW),
        None,
        &signer("ocsp-responder"),
        &[&delegate],
    );
    assert!(ocsp::check(&r, &id, &issuer, now).is_ok());
    // A certificate from the same CA without the OCSP signing usage is no
    // responder.
    let not_delegate = chain("exact.crt")[0].to_vec();
    let r = response(
        &id,
        Status::Good,
        at(NOW),
        None,
        &signer("exact"),
        &[&not_delegate],
    );
    assert_eq!(
        ocsp::check(&r, &id, &issuer, now),
        Err(OcspError::Signature)
    );
    // A responder signed by another CA is no responder either.
    let foreign = chain("other.crt")[0].to_vec();
    let r = response(
        &id,
        Status::Good,
        at(NOW),
        None,
        &signer("other"),
        &[&foreign],
    );
    assert_eq!(
        ocsp::check(&r, &id, &issuer, now),
        Err(OcspError::Signature)
    );
    // Responder statuses other than successful, and garbage.
    let try_later = write::sequence(&[&write::enumerated(3)]);
    assert_eq!(
        ocsp::check(&try_later, &id, &issuer, now),
        Err(OcspError::Status(3))
    );
    let not_basic = write::sequence(&[
        &write::enumerated(0),
        &write::explicit(
            0,
            &write::sequence(&[&write::oid(&[0x2b, 1]), &write::octet_string(&[])]),
        ),
    ]);
    assert_eq!(
        ocsp::check(&not_basic, &id, &issuer, now),
        Err(OcspError::NotBasic)
    );
    assert_eq!(
        ocsp::check(b"garbage", &id, &issuer, now),
        Err(OcspError::Malformed)
    );
    for e in [
        OcspError::Malformed,
        OcspError::Status(1),
        OcspError::NotBasic,
        OcspError::Signature,
        OcspError::NoResponse,
        OcspError::Revoked,
        OcspError::Unknown,
        OcspError::Stale,
        OcspError::NotStapleable("no issuer"),
        OcspError::Fetch("x".into()),
    ] {
        assert!(!e.to_string().is_empty());
    }
}

/// A responder whose answers the test sets.
#[derive(Default)]
struct Responder {
    answer: Mutex<Option<Vec<u8>>>,
    requests: AtomicUsize,
    last_request: Mutex<Vec<u8>>,
}

async fn responder(state: Arc<Responder>) -> SocketAddr {
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = socket.accept().await {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req: http::Request<Incoming>| {
                    let state = Arc::clone(&state);
                    async move {
                        assert_eq!(req.method(), http::Method::POST);
                        assert_eq!(req.headers()["content-type"], "application/ocsp-request");
                        let body = req.into_body().collect().await.unwrap().to_bytes();
                        *state.last_request.lock().unwrap() = body.to_vec();
                        state.requests.fetch_add(1, Ordering::SeqCst);
                        let answer = state.answer.lock().unwrap().clone();
                        let resp = match answer {
                            Some(b) => http::Response::new(Full::new(Bytes::from(b))),
                            None => http::Response::builder()
                                .status(503)
                                .body(Full::new(Bytes::new()))
                                .unwrap(),
                        };
                        Ok::<_, Infallible>(resp)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tcp), svc)
                    .await;
            });
        }
    });
    addr
}

/// Records the OCSP response each handshake carried.
#[derive(Debug, Default)]
struct Capture(Mutex<Vec<Vec<u8>>>);

impl ServerCertVerifier for Capture {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        ocsp: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.0.lock().unwrap().push(ocsp.to_vec());
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The staple a new handshake gets (empty for none).
async fn stapled(addr: SocketAddr) -> Vec<u8> {
    let capture = Arc::new(Capture::default());
    let cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::clone(&capture) as Arc<dyn ServerCertVerifier>)
        .with_no_client_auth();
    let tcp = TcpStream::connect(addr).await.unwrap();
    let name = ServerName::try_from("ocsp.example.com").unwrap();
    tokio_rustls::TlsConnector::from(Arc::new(cfg))
        .connect(name, tcp)
        .await
        .unwrap();
    capture.0.lock().unwrap().pop().unwrap()
}

struct Ok200;

impl Handler for Ok200 {
    type Body = Full<Bytes>;

    async fn call(
        self: Arc<Self>,
        _: http::Request<Incoming>,
        _: SocketAddr,
    ) -> http::Response<Full<Bytes>> {
        http::Response::new(Full::new(Bytes::from_static(b"ok")))
    }
}

/// The stapler on a manual clock: a staple fetched and served at once,
/// refreshed halfway through its validity, kept through failed fetches,
/// withdrawn once it expires, and applied straight away to a reloaded
/// snapshot.
#[tokio::test]
async fn the_stapler_fetches_refreshes_keeps_and_withdraws() {
    let state = Arc::new(Responder::default());
    let addr = responder(Arc::clone(&state)).await;
    let (id, _) = ocsp_id();
    let ca = signer("ca");
    let clock = Arc::new(ManualClock::new());
    let wall = Wall::new(Arc::clone(&clock) as Arc<dyn Clock>, at(NOW));
    let staples = Arc::new(Staples::default());
    let shared = Shared {
        tickets: None,
        staples: Some(Arc::clone(&staples)),
        alpn_challenges: None,
    };
    let tls = tls_config(vec![cert("ocsp", &[])]);
    let store = Store::new(Arc::new(Snapshot::load_with(&tls, &shared).unwrap()));
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let edge = socket.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve_tls(
        socket,
        Acceptor::new(Arc::clone(&store), None),
        Arc::new(Ok200),
        ServerLimits::default(),
        async move {
            let _ = stopped.await;
        },
    ));
    let mut stapler = Stapler::new(
        Arc::clone(&store),
        Arc::clone(&staples),
        wall.clone(),
        Some(format!("http://{addr}/ocsp")),
    );

    // Nothing stapled before the first fetch.
    assert_eq!(stapled(edge).await, Vec::<u8>::new());
    let first = response(&id, Status::Good, at(NOW), Some(at(NOW + 7200)), &ca, &[]);
    *state.answer.lock().unwrap() = Some(first.clone());
    let wait = stapler.tick().await;
    assert_eq!(state.requests.load(Ordering::SeqCst), 1);
    assert_eq!(*state.last_request.lock().unwrap(), id.request());
    assert_eq!(wait, Duration::from_secs(3600), "refresh halfway through");
    assert_eq!(stapled(edge).await, first);

    // Not due yet: no fetch.
    clock.advance(Duration::from_secs(1800));
    stapler.tick().await;
    assert_eq!(state.requests.load(Ordering::SeqCst), 1);

    // Due: refreshed with the responder's newer answer.
    clock.advance(Duration::from_secs(1800));
    let second = response(
        &id,
        Status::Good,
        at(NOW + 3600),
        Some(at(NOW + 10_800)),
        &ca,
        &[],
    );
    *state.answer.lock().unwrap() = Some(second.clone());
    stapler.tick().await;
    assert_eq!(state.requests.load(Ordering::SeqCst), 2);
    assert_eq!(stapled(edge).await, second);

    // A reload staples at once from what was fetched.
    store.swap(Arc::new(Snapshot::load_with(&tls, &shared).unwrap()));
    assert_eq!(stapled(edge).await, second);

    // The responder fails: the staple is kept, and retries back off from a
    // minute.
    clock.advance(Duration::from_secs(3600));
    *state.answer.lock().unwrap() = None;
    let wait = stapler.tick().await;
    assert_eq!(state.requests.load(Ordering::SeqCst), 3);
    assert!(
        wait >= Duration::from_secs(60) && wait <= Duration::from_secs(66),
        "{wait:?}"
    );
    assert_eq!(stapled(edge).await, second);
    clock.advance(wait);
    let wait = stapler.tick().await;
    assert_eq!(state.requests.load(Ordering::SeqCst), 4);
    assert!(
        wait >= Duration::from_secs(120) && wait <= Duration::from_secs(132),
        "{wait:?}"
    );

    // A revoked answer is not stapled; the good staple stays until it
    // expires, and then nothing is stapled.
    *state.answer.lock().unwrap() = Some(response(
        &id,
        Status::Revoked,
        at(NOW + 7300),
        None,
        &ca,
        &[],
    ));
    clock.advance(wait);
    stapler.tick().await;
    assert_eq!(stapled(edge).await, second);
    // Past nextUpdate plus the slack.
    clock.advance(Duration::from_secs(4000));
    stapler.tick().await;
    assert_eq!(stapled(edge).await, Vec::<u8>::new());
    assert!(staples.get(&chain("ocsp.crt")[0], wall.now()).is_none());

    let _ = stop.send(());
    server.await.unwrap();
}

/// `run` ticks on its own and wakes when the store is swapped.
#[tokio::test]
async fn run_fetches_for_a_swapped_in_certificate_at_once() {
    let state = Arc::new(Responder::default());
    let addr = responder(Arc::clone(&state)).await;
    let (id, _) = ocsp_id();
    *state.answer.lock().unwrap() = Some(response(
        &id,
        Status::Good,
        SystemTime::now(),
        Some(SystemTime::now() + Duration::from_secs(7200)),
        &signer("ca"),
        &[],
    ));
    let clock = Arc::new(ManualClock::new());
    let shared = Shared {
        tickets: None,
        staples: Some(Arc::new(Staples::default())),
        alpn_challenges: None,
    };
    // First a certificate with no issuer in its chain: nothing to fetch.
    let store = Store::new(Arc::new(
        Snapshot::load_with(&tls_config(vec![cert("exact", &[])]), &shared).unwrap(),
    ));
    let stapler = Stapler::new(
        Arc::clone(&store),
        Arc::clone(shared.staples.as_ref().unwrap()),
        Wall::new(Arc::clone(&clock) as Arc<dyn Clock>, SystemTime::now()),
        Some(format!("http://{addr}/")),
    );
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(
        stapler.run(Arc::clone(&clock) as Arc<dyn Clock>, async move {
            let _ = stopped.await;
        }),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(state.requests.load(Ordering::SeqCst), 0);
    let snapshot =
        Arc::new(Snapshot::load_with(&tls_config(vec![cert("ocsp", &[])]), &shared).unwrap());
    store.swap(Arc::clone(&snapshot));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while snapshot.staple(0).is_none() {
        assert!(std::time::Instant::now() < deadline, "never stapled");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(state.requests.load(Ordering::SeqCst), 1);
    let _ = stop.send(());
    task.await.unwrap();
}

#[tokio::test]
async fn fetch_errors_are_named() {
    let (id, _) = ocsp_id();
    for (url, want) in [
        ("https://ocsp.test/", "only http://"),
        ("http://127.0.0.1:1/", "127.0.0.1:1"),
        ("not a url", "not a url"),
    ] {
        let err = ocsp::fetch(url, id.request())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(want), "{url}: {err}");
    }
    // A responder that answers 503.
    let state = Arc::new(Responder::default());
    let addr = responder(state).await;
    let err = ocsp::fetch(&format!("http://{addr}/"), id.request())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("status 503"), "{err}");
}
