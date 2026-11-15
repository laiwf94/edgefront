//! Telemetry the products share. [`metrics`] is steer's Prometheus
//! registry and its metric families, rendered as steer rendered them.
//!
//! The cache keeps its fixed families in [`crate::metrics`]; the proxy's
//! families carry labels from the configuration (cluster names, endpoint
//! addresses), so they need series created at configuration time, which
//! is what [`metrics::Registry`] adds.

pub mod metrics;
