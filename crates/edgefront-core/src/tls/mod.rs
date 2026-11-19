//! TLS termination on rustls, ported from steer's `internal/tlsedge`.
//!
//! - [`listener`]: a loaded `tls` block ([`Snapshot`]) with its SNI
//!   certificate table, and the [`Acceptor`] that runs handshakes with a
//!   timeout and a cap on how many run at once;
//! - [`store`]: the [`Store`] that publishes the current snapshot, swapped
//!   whole on reload while live connections keep theirs;
//! - [`ocsp`]: OCSP responses fetched, checked and stapled in the
//!   background;
//! - [`tickets`]: session ticket keys rotated hourly and shared by every
//!   snapshot, so resumption survives rotations and reloads;
//! - [`client_auth`]: client certificates verified against a CA bundle,
//!   and the SAN and SPIFFE identity requests carry;
//! - [`der`]: the few X.509 fields the store reads from a certificate.
//!
//! [`crate::listener::serve_tls`] serves HTTP/1.1 over an acceptor; each
//! request carries the connection's [`TlsInfo`] in its extensions.

pub mod client_auth;
pub mod der;
pub mod listener;
pub mod ocsp;
pub mod store;
pub mod tickets;

pub use client_auth::ClientIdentity;
pub use listener::{
    Acceptor, CertInfo, HandshakeResult, LoadError, OnHandshake, Shared, Slot, Snapshot, TlsInfo,
    UnknownServerName,
};
pub use store::Store;
pub use tickets::TicketKeys;
