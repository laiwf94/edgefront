//! Picks an upstream endpoint for each request attempt, ported from steer's
//! `internal/balancer`.
//!
//! Picking happens in two steps, following Envoy's load balancer: first a
//! priority level is chosen from the share of healthy hosts in each level,
//! with an overprovisioning factor and a panic threshold ([`priority`]),
//! then a [`Policy`] picks a host inside that level: smooth weighted round
//! robin ([`rr`]), power of two choices on peak-EWMA cost ([`ewma`]) or
//! Maglev consistent hashing on a request key ([`maglev`]).
//! Policies that draw at random take an [`rng::Rng`], so a seeded run makes
//! the same picks as steer's.

pub mod ewma;
pub mod maglev;
pub mod priority;
pub mod rng;
pub mod rr;

/// Picks one endpoint among candidates, steer's `balancer.Policy`.
pub trait Policy: Send + Sync {
    /// Picks one of `cands`, indices into the endpoints the policy was built
    /// with, never one listed in `tried` (the endpoints this request has
    /// already tried). `key` is the request's hash key, for hashing
    /// policies. Returns `None` when every candidate has been tried.
    fn pick(&self, cands: &[usize], tried: &[usize], key: Option<u64>) -> Option<usize>;
}
