//! cachet: an HTTP caching reverse proxy for the edge.
//!
//! Modules that have moved into the workspace crates are re-exported under
//! their old paths, so `cachet::range` and friends keep working.

pub use edgefront_cache::{
    cache_control, cluster, collapse, headers, object, policy, purge, range, respond, store,
};
pub use edgefront_core::{ids, server};

pub mod admin;
pub mod cli;
pub mod config;
pub mod metrics;
pub mod node;
pub mod origin;
pub mod proxy;
