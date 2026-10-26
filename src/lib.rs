//! cachet: an HTTP caching reverse proxy for the edge.

pub mod cache_control;
pub mod collapse;
pub mod headers;
pub mod ids;
pub mod metrics;
pub mod object;
pub mod origin;
pub mod policy;
pub mod proxy;
pub mod purge;
pub mod range;
pub mod respond;
pub mod store;
