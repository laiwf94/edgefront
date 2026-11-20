//! Client certificates on a TLS listener (mutual TLS).
//!
//! A `tls` block with `client_auth` asks every client for a certificate
//! and verifies it with rustls's WebPKI verifier against the CA bundle in
//! `client_auth.ca_file`: the chain must lead to one of those CAs, every
//! certificate in it must be inside its validity period, and the leaf must
//! allow client authentication. With `required` (the default) a client
//! without a certificate is refused in the handshake; without it, such a
//! client is let through with no identity, and a client that does present
//! one must still present a valid one.
//!
//! A verified certificate becomes the connection's [`ClientIdentity`]: its
//! DNS and URI subject alternative names, and its SPIFFE ID, the one URI
//! SAN with the `spiffe` scheme (the SPIFFE X.509-SVID rule: exactly one,
//! or none). Every request on the connection carries it in its
//! [`TlsInfo`](super::TlsInfo), so routing can admit clients by identity
//! and the proxy can tell the upstream who called.

use std::sync::Arc;

use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::ClientCertVerifier;

use super::der;
use crate::config::ClientAuth;

/// Who a client proved it is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientIdentity {
    /// The DNS names in the certificate's subject alternative names.
    pub dns_names: Vec<String>,
    /// The URIs in its subject alternative names.
    pub uris: Vec<String>,
    /// The SPIFFE ID, when exactly one URI SAN is a `spiffe://` URI.
    pub spiffe_id: Option<String>,
}

impl ClientIdentity {
    /// The identity of a verified leaf certificate.
    pub fn of(leaf: &CertificateDer<'_>) -> ClientIdentity {
        let Ok(fields) = der::cert_fields(leaf) else {
            return ClientIdentity::default();
        };
        let spiffe: Vec<&String> = fields
            .uris
            .iter()
            .filter(|u| {
                u.get(..9)
                    .is_some_and(|s| s.eq_ignore_ascii_case("spiffe://"))
            })
            .collect();
        ClientIdentity {
            spiffe_id: (spiffe.len() == 1).then(|| spiffe[0].clone()),
            dns_names: fields.dns_names,
            uris: fields.uris,
        }
    }
}

/// Whether `id` matches a SPIFFE pattern: an exact ID, or a prefix ending
/// in `/*` that matches every ID below it (`spiffe://td/ns/web/*` matches
/// `spiffe://td/ns/web/sa/frontend`, not `spiffe://td/ns/webby`). The
/// trust domain compares without case, as SPIFFE IDs do; the path exactly.
pub fn spiffe_matches(pattern: &str, id: &str) -> bool {
    let split = |s: &str| -> Option<(String, String)> {
        let rest = s.get(9..)?;
        if !s[..9].eq_ignore_ascii_case("spiffe://") {
            return None;
        }
        let (domain, path) = rest.split_once('/').map_or((rest, ""), |(d, p)| (d, p));
        Some((domain.to_ascii_lowercase(), format!("/{path}")))
    };
    let (Some((pd, pp)), Some((id_d, id_p))) = (split(pattern), split(id)) else {
        return false;
    };
    if pd != id_d {
        return false;
    }
    match pp.strip_suffix("/*") {
        Some(prefix) => id_p.len() > prefix.len() + 1 && id_p.starts_with(&format!("{prefix}/")),
        None => pp == id_p,
    }
}

/// Builds the verifier for `auth`, reading its CA bundle. With `defer`, a
/// required certificate is not demanded in the handshake but checked right
/// after it (see [`Snapshot`](super::Snapshot)): an ACME TLS-ALPN-01
/// validation has no client certificate and must still complete.
pub fn verifier(auth: &ClientAuth, defer: bool) -> Result<Arc<dyn ClientCertVerifier>, String> {
    let file = &auth.ca_file;
    let mut roots = RootCertStore::empty();
    let certs = CertificateDer::pem_file_iter(file)
        .map_err(|e| format!("tls.client_auth.ca_file: {file}: {e}"))?;
    for cert in certs {
        let cert = cert.map_err(|e| format!("tls.client_auth.ca_file: {file}: {e}"))?;
        roots
            .add(cert)
            .map_err(|e| format!("tls.client_auth.ca_file: {file}: {e}"))?;
    }
    if roots.is_empty() {
        return Err(format!(
            "tls.client_auth.ca_file: {file}: no CA certificates"
        ));
    }
    let builder = WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::new(rustls::crypto::ring::default_provider()),
    );
    let builder = if auth.required && !defer {
        builder
    } else {
        builder.allow_unauthenticated()
    };
    builder
        .build()
        .map_err(|e| format!("tls.client_auth.ca_file: {file}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spiffe_patterns() {
        let id = "spiffe://edge.test/ns/web/sa/frontend";
        for (p, want) in [
            ("spiffe://edge.test/ns/web/sa/frontend", true),
            ("SPIFFE://Edge.Test/ns/web/sa/frontend", true),
            ("spiffe://edge.test/ns/web/sa/Frontend", false),
            ("spiffe://edge.test/ns/web/*", true),
            ("spiffe://edge.test/*", true),
            ("spiffe://edge.test/ns/webby/*", false),
            ("spiffe://edge.test/ns/web/sa/frontend/*", false),
            ("spiffe://other.test/ns/web/*", false),
            ("https://edge.test/ns/web/sa/frontend", false),
            ("spiffe:/", false),
        ] {
            assert_eq!(spiffe_matches(p, id), want, "{p}");
        }
        assert!(!spiffe_matches(
            "spiffe://edge.test/*",
            "spiffe://edge.test/"
        ));
        assert!(!spiffe_matches("spiffe://edge.test/*", "not a uri"));
    }
}
