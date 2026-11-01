//! edgefront-core: the runtime and data-plane foundation that the cache,
//! proxy and limiter crates share.
//!
//! It holds what is not specific to any one product: the proxy
//! configuration (steer's schema, in JSON or TOML), request identifiers,
//! the HTTP/1.1 listener with its connection limits and graceful drain,
//! and the pooled HTTP/1.1 client. The metrics registry and admin router
//! join them as they move out of cachet.

pub mod config;
pub mod http1;
pub mod ids;
pub mod listener;
