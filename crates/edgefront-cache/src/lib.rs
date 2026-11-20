//! edgefront-cache: cachet's caching engine as a library.
//!
//! The RFC 9111 building blocks (`Cache-Control`, validators, the `Range`
//! header and the storage policy), the object model, the memory and disk
//! stores, request collapsing, streaming fills that collapsed requests
//! read while they arrive ([`fill`]), purge and its propagation between nodes,
//! the cache's config file sections, and in [`layer`] the cache as a
//! filter on proxy routes. The binary wires them to the runtime in
//! edgefront-core.

pub mod cache_control;
pub mod cluster;
pub mod collapse;
pub mod config;
pub mod fill;
pub mod headers;
pub mod layer;
pub mod object;
pub mod policy;
pub mod purge;
pub mod range;
pub mod respond;
pub mod slice;
pub mod store;
pub mod tier;
