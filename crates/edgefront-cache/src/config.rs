//! cachet's config file (TOML) and the secrets read from the environment.
//!
//! Secrets never live in the file: the admin token and the cluster key come
//! from `CACHET_ADMIN_TOKEN` and `CACHET_CLUSTER_SECRET`, so the file can be
//! committed and shared. Loading, base URLs and secret handling come from
//! `edgefront_core::config::loader`; this module holds the cache's own
//! sections and the checks serde cannot express.

use std::net::SocketAddr;
use std::path::PathBuf;

use edgefront_core::config::loader::{
    self as config, Redacted, Validate, env_secret, parse_base_url,
};
use http::uri::PathAndQuery;
use serde::Deserialize;

use crate::cache_control::is_tchar;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub node_id: String,
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    #[serde(default = "default_admin_listen")]
    pub admin_listen: SocketAddr,
    pub origins: Vec<OriginConfig>,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub health: HealthConfig,
    #[serde(default)]
    pub timeouts: TimeoutConfig,
    #[serde(default)]
    pub limits: LimitConfig,
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
    #[serde(default)]
    pub gossip: GossipConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GossipConfig {
    /// How long a purge is retried and accepted by peers.
    pub max_age_secs: u64,
    pub retry_max_ms: u64,
    pub request_timeout_ms: u64,
    pub queue: usize,
}

impl Default for GossipConfig {
    fn default() -> Self {
        GossipConfig {
            max_age_secs: 600,
            retry_max_ms: 5000,
            request_timeout_ms: 2000,
            queue: 1024,
        }
    }
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8080))
}

fn default_admin_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9090))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginConfig {
    pub name: String,
    /// `http://host:port`, without a path.
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerConfig {
    pub name: String,
    /// The peer's admin listener, `http://host:port`.
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CacheConfig {
    pub memory_bytes: u64,
    pub max_object_bytes: u64,
    pub shards: usize,
    pub max_variants: usize,
    pub heuristic_max_secs: u64,
    pub disk: Option<DiskConfig>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            memory_bytes: 256 << 20,
            max_object_bytes: 8 << 20,
            shards: 16,
            max_variants: 8,
            heuristic_max_secs: 86_400,
            disk: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskConfig {
    pub path: PathBuf,
    pub max_bytes: u64,
    #[serde(default = "default_disk_queue")]
    pub queue: usize,
}

fn default_disk_queue() -> usize {
    1024
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HealthConfig {
    pub path: String,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub rise: u32,
    pub fall: u32,
}

impl Default for HealthConfig {
    fn default() -> Self {
        HealthConfig {
            path: "/healthz".into(),
            interval_ms: 2000,
            timeout_ms: 1000,
            rise: 2,
            fall: 3,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TimeoutConfig {
    pub connect_ms: u64,
    pub response_ms: u64,
    pub body_ms: u64,
    pub client_header_ms: u64,
    pub shutdown_ms: u64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        TimeoutConfig {
            connect_ms: 1000,
            response_ms: 10_000,
            body_ms: 30_000,
            client_header_ms: 10_000,
            shutdown_ms: 10_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitConfig {
    pub max_uri_bytes: usize,
    pub max_request_body_bytes: u64,
    pub max_header_bytes: usize,
}

impl Default for LimitConfig {
    fn default() -> Self {
        LimitConfig {
            max_uri_bytes: 8 * 1024,
            max_request_body_bytes: 10 << 20,
            max_header_bytes: 64 * 1024,
        }
    }
}

/// Secrets from the environment.
#[derive(Clone, Default)]
pub struct Secrets {
    /// Bearer token for `POST /purge`. Without it, the purge API is off.
    pub admin_token: Option<String>,
    /// Key that signs purge messages between peers.
    pub cluster_secret: Option<Vec<u8>>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets")
            .field("admin_token", &Redacted(&self.admin_token))
            .field("cluster_secret", &Redacted(&self.cluster_secret))
            .finish()
    }
}

pub const MIN_SECRET_BYTES: usize = 32;

impl Secrets {
    /// Reads `CACHET_ADMIN_TOKEN` and `CACHET_CLUSTER_SECRET`.
    pub fn from_env() -> Self {
        Secrets {
            admin_token: env_secret("CACHET_ADMIN_TOKEN"),
            cluster_secret: env_secret("CACHET_CLUSTER_SECRET").map(String::into_bytes),
        }
    }
}

fn valid_name(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(is_tchar)
}

impl Config {
    /// Parses and validates a config file.
    pub fn parse(text: &str) -> Result<Self, String> {
        config::parse(text)
    }

    /// Checks everything that serde cannot.
    pub fn validate(&self) -> Result<(), String> {
        if !valid_name(&self.node_id) {
            return Err(
                "node_id must be 1 to 64 token characters (letters, digits, -._~ and similar)"
                    .into(),
            );
        }
        if self.origins.is_empty() {
            return Err("at least one [[origins]] entry is required".into());
        }
        let mut names = std::collections::HashSet::new();
        for o in &self.origins {
            if !valid_name(&o.name) || !names.insert(&o.name) {
                return Err(format!("origin name {:?} must be a unique token", o.name));
            }
            parse_base_url(&o.url)?;
        }
        let mut peer_names = std::collections::HashSet::new();
        for p in &self.peers {
            if !valid_name(&p.name) || !peer_names.insert(&p.name) || p.name == self.node_id {
                return Err(format!(
                    "peer name {:?} must be a unique token other than node_id",
                    p.name
                ));
            }
            parse_base_url(&p.url)?;
        }
        let c = &self.cache;
        if c.memory_bytes < 1 << 20 {
            return Err("cache.memory_bytes must be at least 1 MiB".into());
        }
        if !(1..=256).contains(&c.shards) {
            return Err("cache.shards must be between 1 and 256".into());
        }
        if c.max_object_bytes == 0 || c.max_object_bytes > c.memory_bytes / c.shards as u64 {
            return Err("cache.max_object_bytes must be positive and fit in one shard (memory_bytes / shards)".into());
        }
        if !(1..=64).contains(&c.max_variants) {
            return Err("cache.max_variants must be between 1 and 64".into());
        }
        if let Some(d) = &c.disk
            && (d.max_bytes < c.max_object_bytes || d.queue == 0)
        {
            return Err(
                "cache.disk.max_bytes must hold at least one object and queue must be positive"
                    .into(),
            );
        }
        let h = &self.health;
        if h.path.parse::<PathAndQuery>().is_err() || !h.path.starts_with('/') {
            return Err(format!("health.path {:?} must be an absolute path", h.path));
        }
        if h.interval_ms == 0 || h.timeout_ms == 0 || h.rise == 0 || h.fall == 0 {
            return Err("health interval, timeout, rise and fall must be positive".into());
        }
        let t = &self.timeouts;
        if [t.connect_ms, t.response_ms, t.body_ms, t.client_header_ms].contains(&0) {
            return Err("timeouts must be positive".into());
        }
        let g = &self.gossip;
        if g.max_age_secs < 10 || g.retry_max_ms == 0 || g.request_timeout_ms == 0 || g.queue == 0 {
            return Err(
                "gossip.max_age_secs must be at least 10 and the other gossip settings positive"
                    .into(),
            );
        }
        let l = &self.limits;
        if l.max_uri_bytes < 64 || l.max_header_bytes < 8192 {
            return Err(
                "limits.max_uri_bytes must be at least 64 and max_header_bytes at least 8192"
                    .into(),
            );
        }
        Ok(())
    }

    /// Checks the secrets this config needs.
    pub fn validate_secrets(&self, secrets: &Secrets) -> Result<(), String> {
        match &secrets.cluster_secret {
            None if !self.peers.is_empty() => {
                Err("CACHET_CLUSTER_SECRET is required when peers are configured".into())
            }
            Some(s) if s.len() < MIN_SECRET_BYTES => Err(format!(
                "CACHET_CLUSTER_SECRET must be at least {MIN_SECRET_BYTES} bytes"
            )),
            _ => Ok(()),
        }
    }
}

impl Validate for Config {
    fn validate(&self) -> Result<(), String> {
        Config::validate(self)
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
    fn minimal_config_gets_defaults() {
        let c = Config::parse(MINIMAL).unwrap();
        assert_eq!(c.listen, default_listen());
        assert_eq!(c.admin_listen, default_admin_listen());
        assert_eq!(c.cache.memory_bytes, 256 << 20);
        assert_eq!(c.cache.shards, 16);
        assert!(c.cache.disk.is_none());
        assert_eq!(c.health.fall, 3);
        assert_eq!(c.health.path, "/healthz");
        assert_eq!(c.timeouts.shutdown_ms, 10_000);
        assert_eq!(c.limits.max_header_bytes, 64 * 1024);
        assert_eq!(c.gossip.max_age_secs, 600);
        assert!(c.peers.is_empty());
        assert!(c.validate_secrets(&Secrets::default()).is_ok());
        assert!(Validate::validate(&c).is_ok());
    }

    #[test]
    fn full_config() {
        let text = r#"
            node_id = "edge-a"
            listen = "127.0.0.1:8080"
            admin_listen = "127.0.0.1:9090"
            [[origins]]
            name = "primary"
            url = "http://origin-a:80"
            [[origins]]
            name = "secondary"
            url = "http://origin-b"
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
            connect_ms = 200
            [limits]
            max_uri_bytes = 2048
            [[peers]]
            name = "edge-b"
            url = "http://edge-b:9090"
        "#;
        let c = Config::parse(text).unwrap();
        assert_eq!(c.origins.len(), 2);
        assert_eq!(c.cache.disk.as_ref().unwrap().queue, 1024);
        assert_eq!(c.health.path, "/up");
        assert_eq!(c.timeouts.response_ms, 10_000);
        assert_eq!(c.limits.max_uri_bytes, 2048);
        let secret = Secrets {
            admin_token: None,
            cluster_secret: Some(vec![7; 32]),
        };
        assert!(c.validate_secrets(&secret).is_ok());
        assert!(
            c.validate_secrets(&Secrets::default())
                .unwrap_err()
                .contains("CACHET_CLUSTER_SECRET")
        );
        let short = Secrets {
            admin_token: None,
            cluster_secret: Some(vec![7; 8]),
        };
        assert!(
            c.validate_secrets(&short)
                .unwrap_err()
                .contains("at least 32")
        );
    }

    fn with(extra: &str) -> Result<Config, String> {
        Config::parse(&format!("{MINIMAL}\n{extra}"))
    }

    #[test]
    fn invalid_configs_are_refused() {
        let cases = [
            ("[cache]\nmemory_bytes = 10", "memory_bytes"),
            ("[cache]\nshards = 0", "shards"),
            (
                "[cache]\nmax_object_bytes = 999999999999",
                "max_object_bytes",
            ),
            ("[cache]\nmax_variants = 0", "max_variants"),
            ("[cache.disk]\npath = \"/tmp/x\"\nmax_bytes = 1", "disk"),
            ("[health]\npath = \"nope\"", "health.path"),
            ("[health]\nrise = 0", "rise"),
            ("[timeouts]\nconnect_ms = 0", "timeouts"),
            ("[limits]\nmax_header_bytes = 10", "max_header_bytes"),
            (
                "[[peers]]\nname = \"edge-a\"\nurl = \"http://x:1\"",
                "peer name",
            ),
            (
                "[[peers]]\nname = \"edge-b\"\nurl = \"http://x:1/p\"",
                "must not have a path",
            ),
            (
                "[[origins]]\nname = \"primary\"\nurl = \"http://x:1\"",
                "unique",
            ),
            (
                "[[origins]]\nname = \"other\"\nurl = \"https://x:1\"",
                "must use http://",
            ),
            ("unknown = 1", "unknown field"),
            ("[gossip]\nmax_age_secs = 1", "gossip"),
        ];
        for (extra, want) in cases {
            let err = with(extra).unwrap_err();
            assert!(err.contains(want), "{extra:?}: {err}");
        }
        assert!(
            Config::parse("node_id = \"bad name\"\norigins = []")
                .unwrap_err()
                .contains("node_id")
        );
        assert!(
            Config::parse("node_id = \"a\"\norigins = []")
                .unwrap_err()
                .contains("at least one")
        );
        assert!(Config::parse("not toml at all [").is_err());
    }

    #[test]
    fn names_are_tokens() {
        assert!(valid_name("edge-a.1_~"));
        assert!(valid_name(&"a".repeat(64)));
        assert!(!valid_name(&"a".repeat(65)));
        assert!(!valid_name(""));
        assert!(!valid_name("a b"));
        assert!(!valid_name("a/b"));
    }

    #[test]
    fn secrets_are_not_printed() {
        let s = Secrets {
            admin_token: Some("hunter2".into()),
            cluster_secret: Some(b"x".repeat(40)),
        };
        let printed = format!("{s:?}");
        assert!(!printed.contains("hunter2"));
        assert_eq!(
            printed,
            r#"Secrets { admin_token: Some("<set>"), cluster_secret: Some("<set>") }"#
        );
        assert_eq!(
            format!("{:?}", Secrets::default()),
            "Secrets { admin_token: None, cluster_secret: None }"
        );
    }
}
