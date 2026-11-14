//! The live state `GET /clusters` reports: steer's `proxy.State`, with its
//! clusters, their balancer picture and every endpoint's health and
//! ejection state, in the JSON shape steer wrote.
//!
//! The proxy fills these in from its own types; this crate only knows the
//! shape, so the admin API does not depend on the proxy.

use std::time::SystemTime;

use super::go_json::Json;

/// The proxy's current view (steer's `proxy.State`).
#[derive(Debug, Clone, PartialEq)]
pub struct ProxyState {
    /// The version of the configuration in force, 1 for the first load.
    pub config_version: u64,
    /// When that configuration was applied.
    pub loaded_at: SystemTime,
    /// The clusters, in configuration order.
    pub clusters: Vec<ClusterState>,
}

/// One cluster (steer's `proxy.ClusterState`).
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterState {
    pub name: String,
    /// The balancing policy as the configuration names it.
    pub policy: String,
    /// Requests holding one of the cluster's `max_requests` permits.
    pub in_flight: i64,
    pub max_requests: u64,
    /// The retry budget's balance; `None`, and left out, without a budget.
    pub retry_budget_balance: Option<f64>,
    pub balancer: BalancerState,
    pub endpoints: Vec<EndpointState>,
}

/// The priority picture of a cluster (steer's `balancer.State`): one entry
/// per priority level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalancerState {
    pub healthy_per_priority: Vec<u64>,
    pub total_per_priority: Vec<u64>,
    /// All zeros in panic and when nothing is available.
    pub priority_load_percent: Vec<u64>,
    pub panic: bool,
}

/// One endpoint (steer's `proxy.EndpointState` with the embedded
/// `endpoint.Snapshot`).
#[derive(Debug, Clone, PartialEq)]
pub struct EndpointState {
    pub address: String,
    pub weight: u64,
    pub priority: u64,
    /// The active health check's verdict.
    pub healthy: bool,
    /// Why the last failed probe failed; empty, and left out, otherwise.
    pub last_probe_error: String,
    pub ejected: bool,
    /// When the current or last ejection ends; left out if it never was.
    pub ejected_until: Option<SystemTime>,
    /// The ejection multiplier.
    pub times_ejected: u64,
    /// steer's reason for the last ejection; empty, and left out, if none.
    pub last_eject_reason: String,
    pub consecutive_gateway_failures: u64,
    pub in_flight: i64,
    /// The latency estimate in milliseconds, rounded to the microsecond.
    pub ewma_latency_ms: f64,
    /// Healthy and not ejected.
    pub available: bool,
}

fn uints(v: &[u64]) -> Json {
    // The balancer's slices are made with `make`, so never nil.
    Json::Array(v.iter().map(|&n| Json::Uint(n)).collect())
}

impl ProxyState {
    /// The document, members in steer's field order.
    pub fn to_json(&self) -> Json {
        Json::object([
            ("config_version", Json::Uint(self.config_version)),
            ("loaded_at", Json::time(self.loaded_at)),
            (
                "clusters",
                // Appended to one by one, so nil while empty.
                Json::slice(
                    self.clusters.iter().map(ClusterState::to_json).collect(),
                    true,
                ),
            ),
        ])
    }
}

impl ClusterState {
    pub fn to_json(&self) -> Json {
        let mut m = vec![
            ("name", Json::str(&self.name)),
            ("policy", Json::str(&self.policy)),
            ("in_flight", Json::Int(self.in_flight)),
            ("max_requests", Json::Uint(self.max_requests)),
        ];
        if let Some(b) = self.retry_budget_balance {
            m.push(("retry_budget_balance", Json::Float(b)));
        }
        m.push(("balancer", self.balancer.to_json()));
        m.push((
            "endpoints",
            Json::slice(
                self.endpoints.iter().map(EndpointState::to_json).collect(),
                true,
            ),
        ));
        Json::object(m)
    }
}

impl BalancerState {
    pub fn to_json(&self) -> Json {
        Json::object([
            ("healthy_per_priority", uints(&self.healthy_per_priority)),
            ("total_per_priority", uints(&self.total_per_priority)),
            ("priority_load_percent", uints(&self.priority_load_percent)),
            ("panic", Json::Bool(self.panic)),
        ])
    }
}

impl EndpointState {
    pub fn to_json(&self) -> Json {
        let mut m = vec![
            ("address", Json::str(&self.address)),
            ("weight", Json::Uint(self.weight)),
            ("priority", Json::Uint(self.priority)),
            ("healthy", Json::Bool(self.healthy)),
        ];
        if !self.last_probe_error.is_empty() {
            m.push(("last_probe_error", Json::str(&self.last_probe_error)));
        }
        m.push(("ejected", Json::Bool(self.ejected)));
        if let Some(t) = self.ejected_until {
            m.push(("ejected_until", Json::time(t)));
        }
        m.push(("times_ejected", Json::Uint(self.times_ejected)));
        if !self.last_eject_reason.is_empty() {
            m.push(("last_eject_reason", Json::str(&self.last_eject_reason)));
        }
        m.extend([
            (
                "consecutive_gateway_failures",
                Json::Uint(self.consecutive_gateway_failures),
            ),
            ("in_flight", Json::Int(self.in_flight)),
            ("ewma_latency_ms", Json::Float(self.ewma_latency_ms)),
            ("available", Json::Bool(self.available)),
        ]);
        Json::object(m)
    }
}
