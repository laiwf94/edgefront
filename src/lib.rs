//! cachet: an HTTP caching reverse proxy for the edge.
//!
//! Modules that have moved into the workspace crates are re-exported under
//! their old paths, so `cachet::range` and friends keep working.

pub use edgefront_cache::{cache_control, range};
pub use edgefront_core::{ids, server};

pub mod admin;
pub mod cli;
pub mod cluster;
pub mod collapse;
pub mod config;
pub mod headers;
pub mod metrics;
pub mod node;
pub mod object;
pub mod origin;
pub mod policy;
pub mod proxy;
pub mod purge;
pub mod respond;
pub mod store;
