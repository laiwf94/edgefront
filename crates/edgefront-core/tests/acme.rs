//! ACME against Pebble in Docker (never a real CA): the account, an order
//! validated over HTTP-01, the certificate checked against Pebble's root,
//! the account reused from its state directory, and the errors a failed
//! challenge, a refused name and an untrusted directory give.

#[path = "common/pebble.rs"]
mod pebble;
#[path = "common/tls.rs"]
mod tlsc;

use std::sync::Arc;
use std::time::Duration;

use edgefront_core::acme::challenges::Http01;
use edgefront_core::acme::client::{self, AccountStore, AcmeError, Client, Issued};
use edgefront_core::listener::{ServerLimits, serve, serve_tls};
use edgefront_core::tls::{Acceptor, Snapshot, Store};
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
        let name = rustls::pki_types::ServerName::try_from(d.to_string()).unwrap();
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
