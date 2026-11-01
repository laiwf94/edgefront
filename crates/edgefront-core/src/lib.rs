//! edgefront-core: the runtime and data-plane foundation that the cache,
//! proxy and limiter crates share.
//!
//! It holds what is not specific to any one product:
//!
//! - [`config`]: the proxy configuration (steer's schema, in JSON or
//!   TOML), and in [`config::loader`] the TOML loading, validation and
//!   environment secrets the cache's config builds on;
//! - [`ids`]: request identifiers;
//! - [`listener`] and [`http1`]: the HTTP/1.1 listener with its connection
//!   limits and graceful drain, and the pooled HTTP/1.1 client;
//! - [`server`]: cachet's accept loop, kept until the cache moves onto the
//!   listener;
//! - [`body`]: the boxed response body handlers return;
//! - [`metrics`]: atomic counters and histograms in the Prometheus text
//!   format;
//! - [`admin`]: the admin router, bearer tokens and JSON errors.

pub mod admin;
pub mod body;
pub mod config;
pub mod http1;
pub mod ids;
pub mod listener;
pub mod metrics;
pub mod server;
