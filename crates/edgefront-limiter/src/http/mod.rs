//! HTTP rate limiting rules for the proxy pipeline: limits keyed on any
//! request field ([`key`]), attached to routes as a filter ([`layer`]),
//! counting only chosen response statuses ([`response_count`]), and the
//! standard `RateLimit-Policy`, `RateLimit` and `Retry-After` headers
//! ([`headers`]). The rules are read from the `[[http_limits]]` section of
//! the limiter config ([`crate::config::http_limits`]).
//!
//! Built with the `http` feature (on by default), which brings in the
//! proxy's crates; without it the limiter is the algorithms alone.

pub mod headers;
pub mod key;
pub mod layer;
pub mod response_count;

pub use key::{Key, KeySpec, Selector, View};
pub use layer::{Action, HttpLimiter, HttpRule, Match, Refusal, RuleStats};
pub use response_count::StatusSet;
