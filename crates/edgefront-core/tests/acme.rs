//! ACME against Pebble in Docker (never a real CA): the account, an order
//! validated by each challenge type, the certificate checked against
//! Pebble's root, the account reused from its state directory; and the
//! renewal scheduler on a manual clock over 180 days.

#[path = "common/pebble.rs"]
mod pebble;
#[path = "common/tls.rs"]
mod tlsc;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use edgefront_core::acme::challenges::{AlpnChallenges, BoxFuture, Dns01, Http01, TlsAlpn01};
use edgefront_core::acme::client::{self, AccountStore, AcmeError, Client, Issued};
use edgefront_core::acme::renew::{Issuer, Renewer, renew_at};
use edgefront_core::clock::{Clock, ManualClock};
use edgefront_core::config::{Acme, AcmeChallenge};
use edgefront_core::listener::{ServerLimits, serve, serve_tls};
use edgefront_core::tls::ocsp::Wall;
use edgefront_core::tls::{Acceptor, Shared, Snapshot, Store};
use pebble::{Pebble, free_port};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use tokio::net::{TcpListener, TcpStream};

const DOMAIN: &str = "edge.test";

/// The account client for `p`, its key in `state`.
async fn account(p: &Pebble, state: &AccountStore) -> Client {
    let key = Arc::new(state.account_key().unwrap());
    let kid = state.kid(&p.directory).unwrap();
    Client::connect(
        &p.directory,
        client::trust(p.api_ca.to_str().unwrap()).unwrap(),
        key,
        kid,
    )
    .await
    .unwrap()
}

/// Checks an issued certificate: it names `domains`, lasts `validity`,
/// and a client trusting only Pebble's root accepts it for each name.
async fn check(p: &Pebble, issued: &Issued, domains: &[&str], validity: Duration) {
    let chain: Vec<CertificateDer<'static>> =
        CertificateDer::pem_slice_iter(issued.chain_pem.as_bytes())
            .map(Result::unwrap)
            .collect();
    assert!(chain.len() >= 2, "a leaf and an intermediate");
    let fields = edgefront_core::tls::der::cert_fields(&chain[0]).unwrap();
    let mut names = fields.dns_names.clone();
    names.sort();
    let mut want: Vec<String> = domains.iter().map(|d| (*d).to_owned()).collect();
    want.sort();
    assert_eq!(names, want);
    let life = issued.not_after.duration_since(issued.not_before).unwrap();
    assert!(
        life.abs_diff(validity) < Duration::from_secs(120),
        "{life:?}"
    );

    // Serve it and connect trusting only Pebble's root.
    let dir = std::env::temp_dir().join(format!(
        "efacme-check-{}-{}",
        std::process::id(),
        domains[0]
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("cert.pem"), &issued.chain_pem).unwrap();
    std::fs::write(dir.join("key.pem"), &issued.key_pem).unwrap();
    let tls = tlsc::tls_config(vec![edgefront_core::config::Certificate {
        cert_file: dir.join("cert.pem").display().to_string(),
        key_file: dir.join("key.pem").display().to_string(),
        server_names: None,
    }]);
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve_tls(
        socket,
        Acceptor::new(Store::new(Arc::new(Snapshot::load(&tls).unwrap())), None),
        Arc::new(Http01::default()),
        ServerLimits::default(),
        async move {
            let _ = stopped.await;
        },
    ));
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_file(p.root_file()).unwrap())
        .unwrap();
    let cfg = Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth(),
    );
    for d in domains {
        let tcp = TcpStream::connect(addr).await.unwrap();
        // A wildcard covers any one label.
        let host = d.replace("*.", "any.");
        let name = rustls::pki_types::ServerName::try_from(host).unwrap();
        tokio_rustls::TlsConnector::from(Arc::clone(&cfg))
            .connect(name, tcp)
            .await
            .unwrap_or_else(|e| panic!("{d}: {e}"));
    }
    let _ = stop.send(());
    let _ = std::fs::remove_dir_all(&dir);
}

fn state(name: &str) -> AccountStore {
    let dir = std::env::temp_dir().join(format!("efacme-state-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    AccountStore::new(dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http01_issues_and_the_account_is_reused() {
    let port = free_port();
    let p = Pebble::start(port, free_port(), 90 * 86_400);
    let responder = Http01::new();
    let socket = TcpListener::bind(("0.0.0.0", port)).await.unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve(
        socket,
        Arc::clone(&responder),
        ServerLimits::default(),
        async move {
            let _ = stopped.await;
        },
    ));
    let store = state("http01");
    let c = account(&p, &store).await;
    let contact = vec!["mailto:ops@edge.test".to_owned()];
    let domains = vec![DOMAIN.to_owned(), format!("www.{DOMAIN}")];
    let issued = client::issue(&c, &contact, &domains, &responder)
        .await
        .unwrap();
    check(
        &p,
        &issued,
        &[DOMAIN, "www.edge.test"],
        Duration::from_secs(90 * 86_400),
    )
    .await;
    // The challenge is gone once the CA has decided.
    assert_eq!(responder.answer("/.well-known/acme-challenge/x"), None);

    // The account key and URL persist; a second client finds the same
    // account and issues again without a new account.
    let kid = c.kid().unwrap();
    store.save_kid(&p.directory, &kid).unwrap();
    store.save_issued(&issued).unwrap();
    let again = account(&p, &store).await;
    assert_eq!(again.kid().as_deref(), Some(kid.as_str()));
    let second = client::issue(&again, &contact, &[DOMAIN.to_owned()], &responder)
        .await
        .unwrap();
    assert_eq!(again.kid().as_deref(), Some(kid.as_str()));
    check(&p, &second, &[DOMAIN], Duration::from_secs(90 * 86_400)).await;
    // And a new key would make a new account.
    let other = Client::connect(
        &p.directory,
        client::trust(p.api_ca.to_str().unwrap()).unwrap(),
        Arc::new(client::Key::generate().unwrap()),
        None,
    )
    .await
    .unwrap();
    assert_ne!(other.account(&contact).await.unwrap(), kid);
    let _ = stop.send(());
    let _ = std::fs::remove_dir_all(store.dir());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_alpn01_issues_through_the_tls_listener() {
    let port = free_port();
    let p = Pebble::start(free_port(), port, 90 * 86_400);
    // A TLS listener with no certificate yet, waiting for ACME: only
    // acme-tls/1 handshakes for a name with a challenge get anywhere.
    let challenges = AlpnChallenges::new();
    let mut tls = tlsc::tls_config(Vec::new());
    tls.acme = Some(Acme {
        directory: p.directory.clone(),
        contact: Vec::new(),
        domains: vec![DOMAIN.to_owned()],
        challenge: AcmeChallenge::TlsAlpn01,
        state_dir: "/unused".into(),
        ca_file: None,
        http_listen: None,
        dns_hook: None,
    });
    let shared = Shared {
        tickets: None,
        staples: None,
        alpn_challenges: Some(Arc::clone(&challenges)),
    };
    let store = Store::new(Arc::new(Snapshot::load_with(&tls, &shared).unwrap()));
    let socket = TcpListener::bind(("0.0.0.0", port)).await.unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve_tls(
        socket,
        Acceptor::new(store, None),
        Arc::new(Http01::default()),
        ServerLimits::default(),
        async move {
            let _ = stopped.await;
        },
    ));
    let accounts = state("alpn");
    let c = account(&p, &accounts).await;
    let issued = client::issue(
        &c,
        &[],
        &[DOMAIN.to_owned()],
        &TlsAlpn01(Arc::clone(&challenges)),
    )
    .await
    .unwrap();
    check(&p, &issued, &[DOMAIN], Duration::from_secs(90 * 86_400)).await;
    assert!(
        challenges.get(DOMAIN).is_none(),
        "the challenge certificate is removed"
    );
    let _ = stop.send(());
    let _ = std::fs::remove_dir_all(accounts.dir());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dns01_issues_a_wildcard_through_the_hook() {
    let p = Pebble::start(free_port(), free_port(), 90 * 86_400);
    let hook = Dns01::new(p.dns_hook());
    let accounts = state("dns");
    let c = account(&p, &accounts).await;
    let domains = vec![DOMAIN.to_owned(), format!("*.{DOMAIN}")];
    let issued = client::issue(&c, &[], &domains, &hook).await.unwrap();
    check(
        &p,
        &issued,
        &[DOMAIN, "*.edge.test"],
        Duration::from_secs(90 * 86_400),
    )
    .await;
    let _ = std::fs::remove_dir_all(accounts.dir());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_challenge_and_protocol_errors_are_reported() {
    let p = Pebble::start(free_port(), free_port(), 90 * 86_400);
    let accounts = state("fail");
    let c = account(&p, &accounts).await;
    // Nothing listens on Pebble's HTTP-01 port: the authorization fails
    // and the error says why.
    let err = client::issue(&c, &[], &[DOMAIN.to_owned()], &Http01::new())
        .await
        .unwrap_err();
    assert!(matches!(err, AcmeError::Invalid(_)), "{err}");
    assert!(err.to_string().contains("edge.test"), "{err}");
    // A name Pebble refuses.
    let err = client::issue(
        &c,
        &[],
        &["blocked-domain.example".to_owned()],
        &Http01::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, AcmeError::Problem { .. }), "{err}");
    // A directory over TLS the trust does not cover.
    let err = Client::connect(
        &p.directory,
        client::trust(&tlsc::fixture("ca.crt")).unwrap(),
        Arc::new(client::Key::generate().unwrap()),
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, AcmeError::Transport(_)), "{err}");
    assert!(
        Client::connect(
            "http://localhost/dir",
            client::trust(p.api_ca.to_str().unwrap()).unwrap(),
            Arc::new(client::Key::generate().unwrap()),
            None
        )
        .await
        .is_err()
    );
    let _ = std::fs::remove_dir_all(accounts.dir());
}

/// An issuer on a fake CA: every certificate lasts 90 days from the wall
/// time of its issuance, and the attempts listed in `fail` fail.
struct FakeCa {
    wall: Wall,
    attempts: AtomicUsize,
    fail: Vec<usize>,
    issued_at: Mutex<Vec<SystemTime>>,
    installed: AtomicUsize,
}

/// The renewer's handle on a shared [`FakeCa`].
struct Ca(Arc<FakeCa>);

impl Issuer for Ca {
    fn issue(&self) -> BoxFuture<'_, Result<Issued, AcmeError>> {
        Box::pin(async move {
            let n = self.0.attempts.fetch_add(1, Ordering::SeqCst);
            if self.0.fail.contains(&n) {
                return Err(AcmeError::Transport("the CA is down".into()));
            }
            let now = self.0.wall.now();
            self.0.issued_at.lock().unwrap().push(now);
            Ok(Issued {
                chain_pem: String::new(),
                key_pem: String::new(),
                not_before: now,
                not_after: now + Duration::from_secs(90 * 86_400),
            })
        })
    }

    fn install(&self, _: &Issued) -> Result<(), AcmeError> {
        self.0.installed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

const DAY: Duration = Duration::from_secs(86_400);

/// 180 days on a manual clock: issued at once, renewed at days 60, 120 and
/// 180 (two thirds of each 90-day lifetime), never expired.
#[tokio::test]
async fn renewal_at_two_thirds_over_180_days() {
    let clock = Arc::new(ManualClock::new());
    let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let wall = Wall::new(Arc::clone(&clock) as Arc<dyn Clock>, start);
    let ca = Arc::new(FakeCa {
        wall: wall.clone(),
        attempts: AtomicUsize::new(0),
        fail: Vec::new(),
        issued_at: Mutex::new(Vec::new()),
        installed: AtomicUsize::new(0),
    });
    let renewer = Renewer::new(
        Ca(Arc::clone(&ca)),
        Arc::clone(&clock) as Arc<dyn Clock>,
        wall.clone(),
        None,
    );
    let status = renewer.status();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(renewer.run(async move {
        let _ = stopped.await;
    }));
    settle().await;
    for _ in 0..180 {
        clock.advance(DAY);
        settle().await;
        let s = status.lock().unwrap().clone();
        let left = s.expires_in(wall.now()).unwrap();
        assert!(
            left > 29.0 * 86_400.0,
            "the certificate got within {left}s of expiry"
        );
    }
    let days: Vec<u64> = ca
        .issued_at
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.duration_since(start).unwrap().as_secs() / 86_400)
        .collect();
    assert_eq!(days, vec![0, 60, 120, 180]);
    assert_eq!(ca.installed.load(Ordering::SeqCst), 4);
    let s = status.lock().unwrap().clone();
    assert_eq!((s.renewals, s.failures), (4, 0));
    assert_eq!(s.next, Some(start + 240 * DAY));
    let mut metrics = String::new();
    s.write_metrics(wall.now(), &mut metrics);
    assert!(
        metrics.contains("edgefront_acme_renewals_total{result=\"success\"} 4"),
        "{metrics}"
    );
    assert!(
        metrics.contains(&format!(
            "edgefront_acme_certificate_expiry_seconds {}",
            90 * 86_400
        )),
        "{metrics}"
    );
    let _ = stop.send(());
    task.await.unwrap();
    assert_eq!(renew_at(start, start + 90 * DAY), start + 60 * DAY);
}

/// A CA down at renewal time: retries after about 5, 10 and 20 minutes,
/// each with at most 10% jitter, then success; the certificate never
/// lapses.
#[tokio::test]
async fn failed_renewals_back_off_and_recover() {
    let clock = Arc::new(ManualClock::new());
    let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let wall = Wall::new(Arc::clone(&clock) as Arc<dyn Clock>, start);
    let ca = Arc::new(FakeCa {
        wall: wall.clone(),
        attempts: AtomicUsize::new(0),
        fail: vec![0, 1, 2],
        issued_at: Mutex::new(Vec::new()),
        installed: AtomicUsize::new(0),
    });
    // Start from a certificate issued a day ago.
    let current = (start - DAY, start + 89 * DAY);
    let renewer = Renewer::new(
        Ca(Arc::clone(&ca)),
        Arc::clone(&clock) as Arc<dyn Clock>,
        wall.clone(),
        Some(current),
    );
    let status = renewer.status();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(renewer.run(async move {
        let _ = stopped.await;
    }));
    settle().await;
    assert_eq!(ca.attempts.load(Ordering::SeqCst), 0, "nothing due yet");
    assert_eq!(status.lock().unwrap().next, Some(start + 59 * DAY));
    clock.advance(59 * DAY);
    settle().await;
    assert_eq!(ca.attempts.load(Ordering::SeqCst), 1);
    // Step a minute at a time and record when each attempt happened.
    let mut at = vec![];
    let mut seen = 1;
    for minute in 1..=60 {
        clock.advance(Duration::from_secs(60));
        settle().await;
        let n = ca.attempts.load(Ordering::SeqCst);
        if n > seen {
            at.push(minute);
            seen = n;
        }
    }
    // Attempts 2, 3 and 4 after backoffs of 5, 10 and 20 minutes (+10%).
    assert_eq!(at.len(), 3, "{at:?}");
    assert!((5..=6).contains(&at[0]), "{at:?}");
    assert!((15..=17).contains(&at[1]), "{at:?}");
    assert!((35..=39).contains(&at[2]), "{at:?}");
    let s = status.lock().unwrap().clone();
    assert_eq!((s.renewals, s.failures), (1, 3));
    assert_eq!(s.last_error, None);
    assert_eq!(ca.installed.load(Ordering::SeqCst), 1);
    let _ = stop.send(());
    task.await.unwrap();
}

#[test]
fn config_checks_the_acme_block() {
    use edgefront_core::config::Config;
    let parse = |acme: &str| {
        Config::from_json(
            format!(
                r#"{{"listen":":1","routes":[{{"cluster":"a"}}],
                "clusters":[{{"name":"a","endpoints":[{{"address":"h:1"}}]}}],
                "tls":{{"certificates":[],"acme":{acme}}}}}"#
            )
            .as_bytes(),
        )
    };
    let ok = parse(
        r#"{"directory":"https://ca/dir","contact":["mailto:ops@edge.test"],"domains":["Edge.Test"],
        "challenge":"http-01","state_dir":"/var/lib/edgefront/acme","http_listen":":80"}"#,
    )
    .unwrap();
    let a = ok.tls.unwrap().acme.unwrap();
    assert_eq!(a.domains, vec!["edge.test".to_owned()]);
    assert_eq!(a.challenge, AcmeChallenge::Http01);
    assert_eq!(a.http_listen.as_deref(), Some(":80"));
    let dns = parse(
        r#"{"directory":"https://ca/dir","domains":["*.edge.test"],"challenge":"dns-01",
        "state_dir":"/s","dns_hook":["/bin/hook"]}"#,
    )
    .unwrap();
    assert_eq!(
        dns.tls.unwrap().acme.unwrap().challenge,
        AcmeChallenge::Dns01
    );
    for (acme, want) in [
        (
            r#"{"directory":"http://ca/dir","domains":["a.test"],"state_dir":"/s","http_listen":":80"}"#,
            "must be an https:// URL",
        ),
        (
            r#"{"directory":"https://ca","domains":[],"state_dir":"/s","http_listen":":80"}"#,
            "at least one domain",
        ),
        (
            r#"{"directory":"https://ca","domains":["a.test"],"http_listen":":80"}"#,
            "state_dir: is required",
        ),
        (
            r#"{"directory":"https://ca","domains":["a.test"],"state_dir":"/s"}"#,
            "http_listen: is required for http-01",
        ),
        (
            r#"{"directory":"https://ca","domains":["*.a.test"],"state_dir":"/s","challenge":"tls-alpn-01"}"#,
            "wildcard names can only be validated with dns-01",
        ),
        (
            r#"{"directory":"https://ca","domains":["a.test"],"state_dir":"/s","challenge":"dns-01"}"#,
            "dns_hook: is required for dns-01",
        ),
        (
            r#"{"directory":"https://ca","domains":["a.test"],"state_dir":"/s","challenge":"magic"}"#,
            "must be http-01, tls-alpn-01 or dns-01",
        ),
        (
            r#"{"directory":"https://ca","domains":["-x-"],"state_dir":"/s","challenge":"tls-alpn-01"}"#,
            "domains[0]",
        ),
        (
            r#"{"directory":"https://ca","domains":["a.test"],"contact":["ops@a.test"],"state_dir":"/s","challenge":"tls-alpn-01"}"#,
            "must be a mailto: URL",
        ),
    ] {
        let err = parse(acme).unwrap_err().to_string();
        assert!(err.contains(want), "{want}: {err}");
    }
    // Without acme a tls block still needs a certificate.
    let err = Config::from_json(
        br#"{"listen":":1","routes":[{"cluster":"a"}],"clusters":[{"name":"a","endpoints":[{"address":"h:1"}]}],"tls":{"certificates":[]}}"#,
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("at least one certificate is required"),
        "{err}"
    );
}
