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
//! - [`ids`]: request identifiers;
//! - [`listener`] and [`http1`]: the HTTP/1.1 listener with its connection
//!   limits and graceful drain, which the proxy and the cache both serve
//!   on, and the pooled HTTP/1.1 client;
//! - [`body`]: the boxed response body handlers return;
//! - [`metrics`]: atomic counters and histograms in the Prometheus text
//!   format;
//! - [`admin`]: the admin router, bearer tokens and JSON errors;
//! - [`filter`]: the phases a layer such as the cache runs in on a
//!   proxy route.

pub mod admin;
pub mod body;
pub mod clock;
pub mod config;
pub mod filter;
pub mod http1;
pub mod ids;
pub mod listener;
pub mod metrics;
