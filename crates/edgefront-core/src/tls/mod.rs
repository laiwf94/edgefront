//! TLS termination on rustls, ported from steer's `internal/tlsedge`.
//!
//! - [`listener`]: a loaded `tls` block ([`Snapshot`]) with its SNI
//!   certificate table, and the [`Acceptor`] that runs handshakes with a
//!   timeout and a cap on how many run at once;
//! - [`der`]: the few X.509 fields the store reads from a certificate.
//!
//! [`crate::listener::serve_tls`] serves HTTP/1.1 over an acceptor; each
//! request carries the connection's [`TlsInfo`] in its extensions.

pub mod der;
pub mod listener;

pub use listener::{
    Acceptor, CertInfo, HandshakeResult, LoadError, OnHandshake, Slot, Snapshot, TlsInfo,
    UnknownServerName,
};
