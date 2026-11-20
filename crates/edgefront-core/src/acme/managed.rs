//! A listener's ACME-managed certificate: where it lives, how it joins the
//! listener's certificates, and the [`Issuer`] that orders it from the CA
//! and swaps it in.
//!
//! The managed certificate is `cert.pem` and `key.pem` in the block's
//! `state_dir`. When both exist, [`effective`] adds them to the listener's
//! configured certificates (last, so a configured certificate stays the
//! fallback; first when there is none, so it becomes the fallback), with
//! its own DNS names for SNI. Each successful issuance writes the files at
//! mode 0600, rebuilds the snapshot from the configuration in force, and
//! swaps it into the listener's [`Store`], so handshakes from then on get
//! the new certificate while open connections keep theirs.

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use super::challenges::{AlpnChallenges, BoxFuture, Dns01, Http01, Solver, TlsAlpn01};
use super::client::{self, AccountStore, AcmeError, Client, Issued, SYSTEM_BUNDLES};
use super::renew::Issuer;
use crate::config::{Acme, AcmeChallenge, Certificate, Tls};
use crate::tls::{Shared, Snapshot, Store};

/// `tls` with the managed certificate added, when it is on disk.
pub fn effective(tls: &Tls) -> Tls {
    let mut out = tls.clone();
    if let Some(acme) = &tls.acme {
        let store = AccountStore::new(&acme.state_dir);
        if store.has_issued() {
            out.certificates.push(Certificate {
                cert_file: store.cert_file().display().to_string(),
                key_file: store.key_file().display().to_string(),
                server_names: None,
            });
        }
    }
    out
}

/// The validity of the managed certificate on disk, if there is one.
pub fn current_validity(acme: &Acme) -> Option<(SystemTime, SystemTime)> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    let store = AccountStore::new(&acme.state_dir);
    let pem = std::fs::read(store.cert_file()).ok()?;
    let leaf = CertificateDer::from_pem_slice(&pem).ok()?;
    let f = crate::tls::der::cert_fields(&leaf).ok()?;
    Some((f.not_before, f.not_after))
}

/// The solver for a block's challenge type.
pub fn solver(
    acme: &Acme,
    http: Option<Arc<Http01>>,
    alpn: Option<Arc<AlpnChallenges>>,
) -> Result<Arc<dyn Solver>, AcmeError> {
    let missing = |what: &str| AcmeError::Challenge(format!("{what} is not set up"));
    Ok(match acme.challenge {
        AcmeChallenge::Http01 => Arc::new(http.ok_or_else(|| missing("the http-01 responder"))?),
        AcmeChallenge::TlsAlpn01 => {
            Arc::new(TlsAlpn01(alpn.ok_or_else(|| missing("tls-alpn-01"))?))
        }
        AcmeChallenge::Dns01 => Arc::new(Dns01::new(
            acme.dns_hook.clone().ok_or_else(|| missing("dns_hook"))?,
        )),
    })
}

/// The trust anchors for the CA's directory.
pub fn directory_trust(acme: &Acme) -> Result<Arc<rustls::ClientConfig>, AcmeError> {
    match &acme.ca_file {
        Some(f) => client::trust(f),
        None => SYSTEM_BUNDLES
            .iter()
            .find(|p| std::path::Path::new(p).is_file())
            .ok_or_else(|| AcmeError::State("no system CA bundle found; set acme.ca_file".into()))
            .and_then(|p| client::trust(p)),
    }
}

/// Orders the managed certificate and installs it in a listener.
pub struct AcmeIssuer {
    acme: Acme,
    solver: Arc<dyn Solver>,
    store: Arc<Store>,
    /// The `tls` block in force; a reload replaces it.
    base: Arc<Mutex<Tls>>,
    shared: Shared,
}

impl AcmeIssuer {
    pub fn new(
        acme: Acme,
        solver: Arc<dyn Solver>,
        store: Arc<Store>,
        base: Arc<Mutex<Tls>>,
        shared: Shared,
    ) -> AcmeIssuer {
        AcmeIssuer {
            acme,
            solver,
            store,
            base,
            shared,
        }
    }
}

impl Issuer for AcmeIssuer {
    fn issue(&self) -> BoxFuture<'_, Result<Issued, AcmeError>> {
        Box::pin(async move {
            let state = AccountStore::new(&self.acme.state_dir);
            let key = Arc::new(state.account_key()?);
            let kid = state.kid(&self.acme.directory)?;
            let client =
                Client::connect(&self.acme.directory, directory_trust(&self.acme)?, key, kid)
                    .await?;
            let issued = client::issue(
                &client,
                &self.acme.contact,
                &self.acme.domains,
                &*self.solver,
            )
            .await;
            // Keep the account even when the order failed.
            if let Some(kid) = client.kid() {
                state.save_kid(&self.acme.directory, &kid)?;
            }
            issued
        })
    }

    fn install(&self, issued: &Issued) -> Result<(), AcmeError> {
        AccountStore::new(&self.acme.state_dir).save_issued(issued)?;
        let base = self.base.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let snapshot = Snapshot::load_with(&effective(&base), &self.shared)
            .map_err(|e| AcmeError::State(format!("installing the certificate: {e}")))?;
        self.store.swap(Arc::new(snapshot));
        Ok(())
    }
}
