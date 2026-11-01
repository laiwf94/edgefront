//! The config file (TOML) and the secrets read from the environment.
//!
//! The file's sections and their validation live in
//! `edgefront_cache::config`, and loading in
//! `edgefront_core::config::loader`; both are re-exported here under their
//! old paths. This module turns a validated config into the settings of
//! cachet's runtime parts.

use std::time::Duration;

pub use edgefront_cache::config::*;
pub use edgefront_core::config::loader::parse_base_url;

use crate::cluster::GossipSettings;
use crate::origin::HealthSettings;
use crate::proxy::ProxySettings;
use crate::store::{CacheSettings, DiskSettings};

/// The runtime settings a validated [`Config`] describes.
pub trait Settings {
    fn cache_settings(&self) -> CacheSettings;
    fn health_settings(&self) -> HealthSettings;
    fn gossip_settings(&self) -> GossipSettings;
    fn proxy_settings(&self) -> ProxySettings;
}

impl Settings for Config {
    fn cache_settings(&self) -> CacheSettings {
        let c = &self.cache;
        CacheSettings {
            memory_bytes: usize::try_from(c.memory_bytes).unwrap_or(usize::MAX),
            shards: c.shards,
            max_object_bytes: usize::try_from(c.max_object_bytes).unwrap_or(usize::MAX),
            max_variants: c.max_variants,
            purge_log: 4096,
            disk: c.disk.as_ref().map(|d| DiskSettings {
                path: d.path.clone(),
                max_bytes: d.max_bytes,
                queue: d.queue,
            }),
        }
    }

    fn health_settings(&self) -> HealthSettings {
        let h = &self.health;
        HealthSettings {
            path: h.path.parse().expect("validated"),
            interval: Duration::from_millis(h.interval_ms),
            timeout: Duration::from_millis(h.timeout_ms),
            rise: h.rise,
            fall: h.fall,
        }
    }

    fn gossip_settings(&self) -> GossipSettings {
        let g = &self.gossip;
        GossipSettings {
            max_age: Duration::from_secs(g.max_age_secs),
            retry_max: Duration::from_millis(g.retry_max_ms),
            request_timeout: Duration::from_millis(g.request_timeout_ms),
            queue: g.queue,
            ..GossipSettings::default()
        }
    }

    fn proxy_settings(&self) -> ProxySettings {
        ProxySettings {
            node_id: self.node_id.clone(),
            max_uri_bytes: self.limits.max_uri_bytes,
            max_request_body: self.limits.max_request_body_bytes,
            body_timeout: Duration::from_millis(self.timeouts.body_ms),
            heuristic_max: Duration::from_secs(self.cache.heuristic_max_secs),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        node_id = "edge-a"
        [[origins]]
        name = "primary"
        url = "http://127.0.0.1:9000"
    "#;

    #[test]
    fn minimal_config_gets_default_settings() {
        let c = Config::parse(MINIMAL).unwrap();
        assert_eq!(c.health_settings().fall, 3);
        assert_eq!(c.health_settings().path, "/healthz");
        assert_eq!(c.proxy_settings().node_id, "edge-a");
        assert_eq!(c.proxy_settings().max_uri_bytes, 8 * 1024);
        assert_eq!(
            c.proxy_settings().heuristic_max,
            Duration::from_secs(86_400)
        );
        let cache = c.cache_settings();
        assert_eq!(cache.shards, 16);
        assert_eq!(cache.memory_bytes, 256 << 20);
        assert_eq!(cache.purge_log, 4096);
        assert!(cache.disk.is_none());
        assert_eq!(c.gossip_settings().queue, 1024);
    }

    #[test]
    fn full_config_settings() {
        let text = r#"
            node_id = "edge-a"
            [[origins]]
            name = "primary"
            url = "http://origin-a:80"
            [cache]
            memory_bytes = 67108864
            max_object_bytes = 1048576
            shards = 8
            [cache.disk]
            path = "/var/cache/cachet"
            max_bytes = 1073741824
            [health]
            path = "/up"
            interval_ms = 500
            [timeouts]
            body_ms = 700
            [gossip]
            max_age_secs = 60
            retry_max_ms = 300
        "#;
        let c = Config::parse(text).unwrap();
        let disk = c.cache_settings().disk.unwrap();
        assert_eq!(disk.queue, 1024);
        assert_eq!(disk.max_bytes, 1 << 30);
        assert_eq!(disk.path, std::path::Path::new("/var/cache/cachet"));
        assert_eq!(c.cache_settings().max_object_bytes, 1 << 20);
        assert_eq!(c.health_settings().path, "/up");
        assert_eq!(c.health_settings().interval, Duration::from_millis(500));
        assert_eq!(c.proxy_settings().body_timeout, Duration::from_millis(700));
        let g = c.gossip_settings();
        assert_eq!(g.max_age, Duration::from_secs(60));
        assert_eq!(g.retry_max, Duration::from_millis(300));
        assert_eq!(g.request_timeout, Duration::from_millis(2000));
    }
}
