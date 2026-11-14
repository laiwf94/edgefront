//! edgefront-core: the runtime and data-plane foundation that the cache,
//! proxy and limiter crates share.
//!
//! It holds what is not specific to any one product:
//!
//! - [`config`]: the proxy configuration (steer's schema, in JSON or
//!   TOML), and in [`config::loader`] the TOML loading, validation and
//!   environment secrets the cache's config builds on;
//! - [`clock`]: monotonic time and timers behind a trait, with a manual
//!   clock for deterministic tests;
//! - [`ids`]: request identifiers, and in [`reqid`] steer's rules for
//!   `X-Request-Id`;
//! - [`listener`] and [`http1`]: the HTTP/1.1 listener with its connection
//!   limits and graceful drain, which the proxy and the cache both serve
//!   on, and the pooled HTTP/1.1 client;
//! - [`body`]: the boxed response body handlers return;
//! - [`metrics`]: atomic counters and histograms in the Prometheus text
//!   format;
//! - [`admin`]: the admin router, bearer tokens and JSON errors, and
//!   steer's operator API ([`admin::Api`]) with its JSON in Go's encoding;
//! - [`filter`]: the phases a layer such as the cache runs in on a
//!   proxy route;
//! - [`l4`]: the base TCP relay, pacerd's data path: bidirectional copy
//!   through one fixed buffer per direction, half-close, and connect and
//!   idle timeouts.

pub mod admin;
pub mod body;
pub mod clock;
pub mod config;
pub mod filter;
pub mod http1;
pub mod ids;
pub mod l4;
pub mod listener;
pub mod metrics;
pub mod reqid;
