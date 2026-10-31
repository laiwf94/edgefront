//! edgefront-core: the runtime and data-plane foundation that the cache,
//! proxy and limiter crates share.
//!
//! It holds what is not specific to any one product: request identifiers
//! today, and the listener, config loader, metrics registry and admin
//! router as they move out of cachet.

pub mod ids;
