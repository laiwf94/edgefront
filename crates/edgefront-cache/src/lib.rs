//! edgefront-cache: cachet's caching engine as a library.
//!
//! The RFC 9111 building blocks live here: `Cache-Control` parsing and the
//! `Range` header. The policy, object model, stores, collapsing and purge
//! follow them out of cachet.

pub mod cache_control;
pub mod range;
