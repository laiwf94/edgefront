//! cachet's config file (TOML) and the secrets read from the environment.
//!
//! Secrets never live in the file: the admin token and the cluster key come
//! from `CACHET_ADMIN_TOKEN` and `CACHET_CLUSTER_SECRET`, so the file can be
//! committed and shared. Loading, base URLs and secret handling come from
//! `edgefront_core::config::loader`; this module holds the cache's own
//! sections and the checks serde cannot express.
//!
//! # Cache settings per route
//!
//! The `[cache]` section is also the global block that routes inherit
//! from. Each `[[cache.routes]]` entry names one proxy route by its `host`
//! and `path_prefix` and turns the cache on for it, overriding any of the
//! global `max_object_bytes`, `max_variants`, `heuristic_max_secs` and
//! `max_ttl_secs`; `enabled = false` keeps a route's block but bypasses the
//! cache. A route with no entry bypasses the cache. [`CacheConfig::resolve`]
//! ties the entries to the proxy's routes and gives each route its
//! effective [`RoutePolicy`], or every problem it found, each with the path
//! of its field, such as `cache.routes[2].max_variants`.
//!
//! ```toml
//! [cache]
//! max_ttl_secs = 86400            # inherited by every cached route
//!
//! [[cache.routes]]                # cached with the global settings
//! host = "*"
//! path_prefix = "/static/"
//!
//! [[cache.routes]]                # cached with tighter limits
//! host = "api.example.com"
//! path_prefix = "/v1/catalog"
//! max_object_bytes = 262144
//! max_ttl_secs = 60
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use edgefront_core::config::loader::{
    self as config, Redacted, Validate, env_secret, parse_base_url,
};
use edgefront_core::config::{FieldError, Route, limits};
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
    /// Cap on any stored response's freshness lifetime, inherited by every
    /// route; `None` leaves the origin's lifetime as it is.
    pub max_ttl_secs: Option<u64>,
    pub disk: Option<DiskConfig>,
    /// The routes the cache serves, each with its overrides.
    pub routes: Vec<RouteCacheConfig>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            memory_bytes: 256 << 20,
            max_object_bytes: 8 << 20,
            shards: 16,
            max_variants: 8,
            heuristic_max_secs: 86_400,
            max_ttl_secs: None,
            disk: None,
            routes: Vec::new(),
        }
    }
}

/// The longest freshness lifetime a route may be given or allowed to keep,
/// one year: RFC 9111 section 4.2.1 calls longer lifetimes unlikely, and a
/// cap keeps the arithmetic far from overflow.
pub const MAX_TTL_SECS: u64 = 365 * 86_400;

/// One `[[cache.routes]]` entry: the cache turned on for the proxy route
/// with this `host` and `path_prefix`. Every other field is an override of
/// the global `[cache]` value of the same name.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteCacheConfig {
    pub host: String,
    pub path_prefix: String,
    /// `false` keeps the entry but bypasses the cache for the route.
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub max_object_bytes: Option<u64>,
    #[serde(default)]
    pub max_variants: Option<usize>,
    #[serde(default)]
    pub heuristic_max_secs: Option<u64>,
    #[serde(default)]
    pub max_ttl_secs: Option<u64>,
}

/// What the cache does on one route, with the global values filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutePolicy {
    /// The largest response body stored for the route.
    pub max_object_bytes: u64,
    /// Stored variants per URL when the origin sends `Vary`.
    pub max_variants: usize,
    /// Cap on freshness guessed from `Last-Modified`, already held under
    /// `max_ttl`.
    pub heuristic_max: Duration,
    /// Cap on any stored response's freshness lifetime.
    pub max_ttl: Option<Duration>,
}

/// The effective cache policy of every proxy route, in the routes' order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoutePolicies {
    routes: Vec<Option<RoutePolicy>>,
}

impl RoutePolicies {
    /// The policy of route `index`, or `None` when the route bypasses the
    /// cache (no entry, `enabled = false`, or no such route).
    pub fn get(&self, index: usize) -> Option<&RoutePolicy> {
        self.routes.get(index).and_then(Option::as_ref)
    }

    /// The number of routes, cached or not.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// How many routes go through the cache.
    pub fn cached(&self) -> usize {
        self.routes.iter().filter(|p| p.is_some()).count()
    }
}

/// Collects field errors up to [`limits::MAX_ERRORS`], as the proxy's
/// config validation does.
#[derive(Default)]
struct Errors(Vec<FieldError>);

impl Errors {
    fn push(&mut self, path: impl Into<String>, message: impl Into<String>) {
        if self.0.len() < limits::MAX_ERRORS {
            self.0.push(FieldError {
                path: path.into(),
                message: message.into(),
            });
        }
    }

    fn range<T: PartialOrd + std::fmt::Display>(&mut self, path: &str, v: T, lo: T, hi: T) {
        if v < lo || v > hi {
            self.push(path, format!("must be between {lo} and {hi}, got {v}"));
        }
    }
}

impl CacheConfig {
    /// Checks the global block's route settings and every
    /// `[[cache.routes]]` entry on its own, without the proxy's routes:
    /// value ranges, overrides that exceed what the store holds, and
    /// entries naming the same route twice.
    pub fn route_errors(&self) -> Vec<FieldError> {
        let mut errs = Errors::default();
        self.check_routes(&mut errs);
        errs.0
    }

    fn check_routes(&self, errs: &mut Errors) {
        if let Some(ttl) = self.max_ttl_secs {
            errs.range("cache.max_ttl_secs", ttl, 1, MAX_TTL_SECS);
        }
        if self.heuristic_max_secs > MAX_TTL_SECS {
            errs.push(
                "cache.heuristic_max_secs",
                format!(
                    "must be at most {MAX_TTL_SECS}, got {}",
                    self.heuristic_max_secs
                ),
            );
        }
        if self.routes.len() > limits::MAX_ROUTES {
            errs.push(
                "cache.routes",
                format!(
                    "at most {} entries, got {}",
                    limits::MAX_ROUTES,
                    self.routes.len()
                ),
            );
        }
        let mut seen = std::collections::HashMap::new();
        for (i, r) in self.routes.iter().enumerate() {
            let at = |field: &str| format!("cache.routes[{i}].{field}");
            if r.host.is_empty() {
                errs.push(at("host"), "must not be empty");
            }
            if !r.path_prefix.starts_with('/') {
                errs.push(
                    at("path_prefix"),
                    format!("must start with \"/\", got {:?}", r.path_prefix),
                );
            }
            if let Some(v) = r.max_object_bytes {
                // The store refuses anything larger, whatever the route says.
                errs.range(&at("max_object_bytes"), v, 1, self.max_object_bytes);
            }
            if let Some(v) = r.max_variants {
                errs.range(&at("max_variants"), v, 1, self.max_variants);
            }
            if let Some(v) = r.heuristic_max_secs {
                errs.range(&at("heuristic_max_secs"), v, 0, MAX_TTL_SECS);
            }
            if let Some(v) = r.max_ttl_secs {
                errs.range(&at("max_ttl_secs"), v, 1, MAX_TTL_SECS);
            }
            let key = (r.host.to_ascii_lowercase(), r.path_prefix.as_str());
            if let Some(first) = seen.insert(key, i) {
                errs.push(
                    format!("cache.routes[{i}]"),
                    format!("names the same route as cache.routes[{first}]"),
                );
            }
        }
    }

    /// Ties the `[[cache.routes]]` entries to the proxy's `routes` and
    /// gives each route its effective policy, or every problem found. An
    /// entry matches the route with the same `path_prefix` and the same
    /// `host`, compared without case as host names are. An entry that
    /// matches no route is an error, so a typo cannot silently leave a
    /// route uncached.
    pub fn resolve(&self, routes: &[Route]) -> Result<RoutePolicies, Vec<FieldError>> {
        let mut errs = Errors::default();
        self.check_routes(&mut errs);
        let mut policies = vec![None; routes.len()];
        for (i, entry) in self.routes.iter().enumerate() {
            let Some(index) = routes.iter().position(|r| {
                r.host.eq_ignore_ascii_case(&entry.host) && r.path_prefix == entry.path_prefix
            }) else {
                errs.push(
                    format!("cache.routes[{i}]"),
                    format!(
                        "no route has host {:?} and path_prefix {:?}",
                        entry.host, entry.path_prefix
                    ),
                );
                continue;
            };
            if entry.enabled.unwrap_or(true) {
                policies[index] = Some(self.policy(entry));
            }
        }
        if errs.0.is_empty() {
            Ok(RoutePolicies { routes: policies })
        } else {
            Err(errs.0)
        }
    }

    fn policy(&self, entry: &RouteCacheConfig) -> RoutePolicy {
        let max_ttl = entry
            .max_ttl_secs
            .or(self.max_ttl_secs)
            .map(Duration::from_secs);
        let heuristic =
            Duration::from_secs(entry.heuristic_max_secs.unwrap_or(self.heuristic_max_secs));
        RoutePolicy {
            max_object_bytes: entry.max_object_bytes.unwrap_or(self.max_object_bytes),
            max_variants: entry.max_variants.unwrap_or(self.max_variants),
            heuristic_max: max_ttl.map_or(heuristic, |cap| heuristic.min(cap)),
            max_ttl,
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
        if let Some(e) = c.route_errors().first() {
            return Err(e.to_string());
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

    fn route(host: &str, path_prefix: &str) -> Route {
        Route {
            host: host.into(),
            path_prefix: path_prefix.into(),
            cluster: "web".into(),
        }
    }

    fn proxy_routes() -> Vec<Route> {
        vec![
            route("*", "/static/"),
            route("api.example.com", "/v1/catalog"),
            route("api.example.com", "/"),
            route("*", "/"),
        ]
    }

    /// The `[cache]` section of `MINIMAL` plus `extra`, parsed but not
    /// validated.
    fn cache_section(extra: &str) -> CacheConfig {
        toml::from_str::<Config>(&format!("{MINIMAL}\n{extra}"))
            .unwrap()
            .cache
    }

    fn paths(errs: &[FieldError]) -> Vec<String> {
        errs.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn routes_inherit_the_global_block() {
        let c = cache_section(
            r#"
            [cache]
            max_object_bytes = 1048576
            max_variants = 4
            heuristic_max_secs = 600
            max_ttl_secs = 3600
            [[cache.routes]]
            host = "*"
            path_prefix = "/static/"
            [[cache.routes]]
            host = "API.example.com"
            path_prefix = "/v1/catalog"
            max_object_bytes = 262144
            max_variants = 2
            heuristic_max_secs = 120
            max_ttl_secs = 60
            [[cache.routes]]
            host = "api.example.com"
            path_prefix = "/"
            enabled = false
            "#,
        );
        let p = c.resolve(&proxy_routes()).unwrap();
        assert_eq!(p.len(), 4);
        assert!(!p.is_empty());
        assert_eq!(p.cached(), 2);
        assert_eq!(
            p.get(0),
            Some(&RoutePolicy {
                max_object_bytes: 1 << 20,
                max_variants: 4,
                heuristic_max: Duration::from_secs(600),
                max_ttl: Some(Duration::from_secs(3600)),
            })
        );
        // The heuristic cap never outlasts the route's own lifetime cap.
        assert_eq!(
            p.get(1),
            Some(&RoutePolicy {
                max_object_bytes: 262_144,
                max_variants: 2,
                heuristic_max: Duration::from_secs(60),
                max_ttl: Some(Duration::from_secs(60)),
            })
        );
        // Disabled, without an entry, and out of range all bypass.
        assert_eq!(p.get(2), None);
        assert_eq!(p.get(3), None);
        assert_eq!(p.get(4), None);
    }

    #[test]
    fn without_entries_every_route_bypasses() {
        let c = Config::parse(MINIMAL).unwrap().cache;
        assert!(c.route_errors().is_empty());
        let p = c.resolve(&proxy_routes()).unwrap();
        assert_eq!((p.len(), p.cached()), (4, 0));
        assert!(RoutePolicies::default().is_empty());
        let none: &[Route] = &[];
        assert!(c.resolve(none).unwrap().is_empty());
    }

    #[test]
    fn defaults_apply_without_a_lifetime_cap() {
        let c = cache_section("[[cache.routes]]\nhost = \"*\"\npath_prefix = \"/\"");
        let p = c.resolve(&proxy_routes()).unwrap();
        assert_eq!(p.cached(), 1);
        assert_eq!(
            p.get(3),
            Some(&RoutePolicy {
                max_object_bytes: 8 << 20,
                max_variants: 8,
                heuristic_max: Duration::from_secs(86_400),
                max_ttl: None,
            })
        );
    }

    #[test]
    fn route_errors_name_their_fields() {
        let mut c = cache_section(
            r#"
            [cache]
            max_ttl_secs = 0
            [[cache.routes]]
            host = ""
            path_prefix = "static"
            max_object_bytes = 0
            max_variants = 9
            heuristic_max_secs = 99999999999
            max_ttl_secs = 99999999999
            [[cache.routes]]
            host = "*"
            path_prefix = "/static/"
            max_object_bytes = 8388609
            [[cache.routes]]
            host = "*"
            path_prefix = "/static/"
            max_ttl_secs = 0
            [[cache.routes]]
            host = "nowhere.example"
            path_prefix = "/"
            "#,
        );
        c.heuristic_max_secs = MAX_TTL_SECS + 1;
        let want = [
            "cache.max_ttl_secs: must be between 1 and 31536000, got 0",
            "cache.heuristic_max_secs: must be at most 31536000, got 31536001",
            "cache.routes[0].host: must not be empty",
            "cache.routes[0].path_prefix: must start with \"/\", got \"static\"",
            "cache.routes[0].max_object_bytes: must be between 1 and 8388608, got 0",
            "cache.routes[0].max_variants: must be between 1 and 8, got 9",
            "cache.routes[0].heuristic_max_secs: must be between 0 and 31536000, got 99999999999",
            "cache.routes[0].max_ttl_secs: must be between 1 and 31536000, got 99999999999",
            "cache.routes[1].max_object_bytes: must be between 1 and 8388608, got 8388609",
            "cache.routes[2].max_ttl_secs: must be between 1 and 31536000, got 0",
            "cache.routes[2]: names the same route as cache.routes[1]",
        ];
        assert_eq!(paths(&c.route_errors()), want);
        let errs = c.resolve(&proxy_routes()).unwrap_err();
        let mut all = want.map(String::from).to_vec();
        all.push("cache.routes[0]: no route has host \"\" and path_prefix \"static\"".into());
        all.push(
            "cache.routes[3]: no route has host \"nowhere.example\" and path_prefix \"/\"".into(),
        );
        assert_eq!(paths(&errs), all);
    }

    #[test]
    fn hosts_compare_without_case_and_duplicates_are_refused() {
        let c = cache_section(
            "[[cache.routes]]\nhost = \"API.example.com\"\npath_prefix = \"/\"\n\
             [[cache.routes]]\nhost = \"api.EXAMPLE.com\"\npath_prefix = \"/\"",
        );
        assert_eq!(
            paths(&c.route_errors()),
            ["cache.routes[1]: names the same route as cache.routes[0]"]
        );
        // A path prefix is compared exactly.
        let c = cache_section("[[cache.routes]]\nhost = \"*\"\npath_prefix = \"/Static/\"");
        assert_eq!(
            paths(&c.resolve(&proxy_routes()).unwrap_err()),
            ["cache.routes[0]: no route has host \"*\" and path_prefix \"/Static/\""]
        );
    }

    #[test]
    fn errors_are_capped_and_entries_are_bounded() {
        let c = CacheConfig {
            routes: (0..=limits::MAX_ROUTES)
                .map(|_| RouteCacheConfig {
                    host: "*".into(),
                    path_prefix: "/".into(),
                    enabled: None,
                    max_object_bytes: None,
                    max_variants: None,
                    heuristic_max_secs: None,
                    max_ttl_secs: None,
                })
                .collect(),
            ..CacheConfig::default()
        };
        let errs = c.route_errors();
        assert_eq!(errs.len(), limits::MAX_ERRORS);
        assert_eq!(errs[0].path, "cache.routes");
        assert_eq!(errs[0].message, "at most 1024 entries, got 1025");
        assert_eq!(errs[1].path, "cache.routes[1]");
    }

    #[test]
    fn the_cachet_validator_reports_the_first_route_error() {
        let err = with("[[cache.routes]]\nhost = \"*\"\npath_prefix = \"/\"\nmax_variants = 0")
            .unwrap_err();
        assert_eq!(
            err,
            "cache.routes[0].max_variants: must be between 1 and 8, got 0"
        );
        let err = with("[[cache.routes]]\nhost = \"*\"\npath_prefix = \"/\"\nttl = 1").unwrap_err();
        assert!(err.contains("unknown field `ttl`"), "{err}");
        let err = with("[cache]\nmax_ttl_secs = 0").unwrap_err();
        assert!(err.starts_with("cache.max_ttl_secs:"), "{err}");
    }
}
