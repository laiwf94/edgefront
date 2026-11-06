//! Picks an upstream endpoint for each request attempt, ported from steer's
//! `internal/balancer`.
//!
//! Picking happens in two steps, following Envoy's load balancer: first a
//! priority level is chosen from the share of healthy hosts in each level,
//! with an overprovisioning factor and a panic threshold ([`priority`]),
//! then a policy picks a host inside that level.

pub mod priority;
