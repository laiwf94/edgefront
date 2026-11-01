//! The proxy configuration, ported from steer's `internal/config`.
//!
//! steer's JSON files load unchanged, and the same schema can be written in
//! TOML. Parsing is strict: unknown fields, trailing data, out-of-range
//! values and dangling references are all rejected, and every problem is
//! reported with the path of the field it concerns, such as
//! `clusters[0].retry.backoff_max`. A [`Config`] returned without error has
//! every default applied.
//!
//! Acceptance is steer's, decision for decision, so that a file one accepts
//! the other accepts too. That covers the quirks of Go's JSON decoder (see
//! [`raw`]), Go's duration syntax and its address parsers (see [`go`]); the
//! differential test drives both with generated configs to keep it so.
//!
//! A `Config` serializes back into the same schema, with durations written
//! the way Go prints them (`"1m30s"`), and the result parses into an equal
//! `Config`.

mod go;
pub mod loader;
mod node;
mod raw;
mod validate;

use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Serialize, Serializer};

/// Limits on the size of a configuration.
pub mod limits {
    use std::time::Duration;

    /// The largest configuration file, in bytes.
    pub const MAX_FILE_BYTES: usize = 1 << 20;
    pub const MAX_ROUTES: usize = 1024;
    pub const MAX_CLUSTERS: usize = 256;
    /// Per cluster.
    pub const MAX_ENDPOINTS: usize = 1024;
    pub const MAX_PRIORITY: u8 = 7;
    pub const MAX_WEIGHT: u32 = 1000;
    pub const MAX_TRUSTED_PROXIES: usize = 64;
    pub const MAX_RETRIES: u32 = 5;
    pub const MAX_RETRY_BODY_BYTES: u64 = 16 << 20;
    pub const MAX_PATH_PREFIX_LEN: usize = 1024;
    /// Validation stops listing problems after this many.
    pub const MAX_ERRORS: usize = 20;
    pub const MAX_CERTIFICATES: usize = 256;
    pub const MAX_SERVER_NAMES_PER_CERT: usize = 100;
    pub const MAX_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
    pub const MIN_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(100);
    pub const MAX_CONCURRENT_HANDSHAKE_CAP: u32 = 100_000;
}

/// Values applied when a field is absent.
pub mod defaults {
    use std::time::Duration;

    pub const ADMIN_LISTEN: &str = "127.0.0.1:9901";
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
    pub const TIMEOUT: Duration = Duration::from_secs(15);
    pub const MAX_REQUESTS: u32 = 1024;
    pub const PANIC_THRESHOLD_PERCENT: u8 = 50;

    pub const HEALTH_INTERVAL: Duration = Duration::from_secs(5);
    pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(1);
    pub const HEALTHY_AFTER: u32 = 2;
    pub const UNHEALTHY_AFTER: u32 = 2;

    pub const CONSECUTIVE_GATEWAY_FAILURES: u32 = 5;
    pub const OUTLIER_INTERVAL: Duration = Duration::from_secs(10);
    pub const BASE_EJECTION_TIME: Duration = Duration::from_secs(30);
    pub const MAX_EJECTION_TIME: Duration = Duration::from_secs(300);
    pub const MAX_EJECTION_PERCENT: u8 = 10;
    pub const SUCCESS_RATE_MINIMUM_HOSTS: u32 = 5;
    pub const SUCCESS_RATE_REQUEST_VOLUME: u32 = 100;
    pub const SUCCESS_RATE_STDEV_FACTOR: f64 = 1.9;

    pub const MAX_RETRIES: u32 = 2;
    pub const RETRY_ON: [u16; 3] = [502, 503, 504];
    pub const BACKOFF_BASE: Duration = Duration::from_millis(25);
    pub const BACKOFF_MAX: Duration = Duration::from_millis(250);
    pub const RETRY_BODY_BYTES: u64 = 64 << 10;
    pub const BUDGET_RATIO: f64 = 0.2;
    pub const BUDGET_MIN_PER_SECOND: u32 = 10;
    pub const BUDGET_WINDOW: Duration = Duration::from_secs(10);

    pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
    pub const MAX_CONCURRENT_HANDSHAKES: u32 = 1024;
}

/// A validated configuration with defaults applied.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Config {
    pub listen: String,
    pub admin_listen: String,
    pub trusted_proxies: Vec<IpPrefix>,
    /// `None` serves plain HTTP.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<Tls>,
    pub routes: Vec<Route>,
    pub clusters: Vec<Cluster>,
}

/// An IP network, such as `10.0.0.0/8`. The address has no bits set past
/// the prefix length. A single address is a prefix of its full length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IpPrefix {
    addr: IpAddr,
    len: u8,
}

impl IpPrefix {
    /// The network address.
    pub fn addr(&self) -> IpAddr {
        self.addr
    }

    /// The prefix length in bits.
    pub fn len(&self) -> u8 {
        self.len
    }

    /// Whether this is `/0`, which every address of its family matches.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether `ip` is inside the network. As in Go's `netip`, an IPv4
    /// address is never inside an IPv6 network or the other way round, so
    /// `::ffff:10.0.0.1` is not in `10.0.0.0/8`.
    pub fn contains(&self, ip: IpAddr) -> bool {
        ip.is_ipv4() == self.addr.is_ipv4() && go::mask(ip, self.len) == self.addr
    }
}

impl fmt::Display for IpPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.len)
    }
}

impl Serialize for IpPrefix {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// TLS termination on the proxy listener.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tls {
    pub certificates: Vec<Certificate>,
    pub min_version: TlsVersion,
    /// The TLS 1.2 suites in preference order; `None` when the minimum is
    /// TLS 1.3, whose suites are fixed by the protocol and always enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cipher_suites: Option<Vec<CipherSuite>>,
    #[serde(serialize_with = "go_duration")]
    pub handshake_timeout: Duration,
    pub max_concurrent_handshakes: u32,
    /// Refuse handshakes whose server name matches no certificate (and
    /// handshakes without one) instead of answering with the first
    /// certificate.
    pub strict_sni: bool,
}

/// The lowest TLS version the listener accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TlsVersion {
    #[serde(rename = "1.2")]
    Tls12,
    #[serde(rename = "1.3")]
    Tls13,
}

/// A TLS 1.2 cipher suite by its IANA number and standard name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CipherSuite {
    pub id: u16,
    pub name: &'static str,
}

impl Serialize for CipherSuite {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.name)
    }
}

/// A PEM certificate chain and its private key. Relative paths in a file
/// loaded with [`Config::load`] are resolved against the file's directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Certificate {
    pub cert_file: String,
    pub key_file: String,
    /// Lower-case names or `*.suffix` wildcards that override the names in
    /// the certificate's DNS SANs; `None` uses those.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_names: Option<Vec<String>>,
}

/// Sends requests for `host` whose path starts with `path_prefix` to
/// `cluster`. `host` is `*`, an exact lower-case name, or `*.suffix`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Route {
    pub host: String,
    pub path_prefix: String,
    pub cluster: String,
}

/// A load-balancing policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// Smooth weighted round robin.
    RoundRobin,
    /// Power of two choices on in-flight requests times EWMA latency.
    LeastLoaded,
    /// Consistent hashing on a request header.
    Maglev,
}

/// A named group of endpoints and the policies used to reach them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cluster {
    pub name: String,
    pub policy: Policy,
    /// The canonical header name hashed by [`Policy::Maglev`]; set exactly
    /// when the policy is Maglev.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash_header: Option<String>,
    pub endpoints: Vec<Endpoint>,
    #[serde(serialize_with = "go_duration")]
    pub connect_timeout: Duration,
    /// For the whole request; zero means none.
    #[serde(serialize_with = "go_duration")]
    pub timeout: Duration,
    pub max_requests: u32,
    /// Zero disables panic routing.
    pub panic_threshold_percent: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_check: Option<HealthCheck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outlier_detection: Option<OutlierDetection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<Retry>,
}

/// One upstream address, `host:port`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Endpoint {
    pub address: String,
    pub weight: u32,
    pub priority: u8,
}

/// Active HTTP health checking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthCheck {
    pub path: String,
    /// The Host header of probes; empty uses the endpoint address.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub host: String,
    #[serde(serialize_with = "go_duration")]
    pub interval: Duration,
    #[serde(serialize_with = "go_duration")]
    pub jitter: Duration,
    #[serde(serialize_with = "go_duration")]
    pub timeout: Duration,
    pub healthy_threshold: u32,
    pub unhealthy_threshold: u32,
}

/// Passive, Envoy-style outlier ejection.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutlierDetection {
    /// Zero disables consecutive-failure ejection.
    pub consecutive_gateway_failures: u32,
    #[serde(serialize_with = "go_duration")]
    pub interval: Duration,
    #[serde(serialize_with = "go_duration")]
    pub base_ejection_time: Duration,
    #[serde(serialize_with = "go_duration")]
    pub max_ejection_time: Duration,
    pub max_ejection_percent: u8,
    /// Zero disables success-rate ejection.
    pub success_rate_minimum_hosts: u32,
    pub success_rate_request_volume: u32,
    pub success_rate_stdev_factor: f64,
}

/// Retries for a cluster.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Retry {
    pub max_retries: u32,
    pub retry_on: Vec<u16>,
    /// Time to response headers; zero means none.
    #[serde(serialize_with = "go_duration")]
    pub per_try_timeout: Duration,
    #[serde(serialize_with = "go_duration")]
    pub backoff_base: Duration,
    #[serde(serialize_with = "go_duration")]
    pub backoff_max: Duration,
    pub max_body_bytes: u64,
    pub budget: Budget,
}

/// Limits retries to `ratio` of requests plus `min_per_second`, over
/// `window`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Budget {
    pub ratio: f64,
    pub min_per_second: u32,
    #[serde(serialize_with = "go_duration")]
    pub window: Duration,
}

fn go_duration<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
    // Validated durations are at most a day, far inside i64 nanoseconds.
    let nanos = i64::try_from(d.as_nanos()).unwrap_or(i64::MAX);
    s.serialize_str(&go::format_duration(nanos))
}

/// The syntax of a configuration file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Toml,
}

impl Format {
    /// TOML for a `.toml` file, JSON for anything else, as steer read every
    /// file as JSON.
    pub fn from_path(path: &Path) -> Format {
        match path.extension() {
            Some(ext) if ext.eq_ignore_ascii_case("toml") => Format::Toml,
            _ => Format::Json,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Format::Json => "JSON",
            Format::Toml => "TOML",
        }
    }
}

/// One problem found while validating, at a field path such as
/// `clusters[0].endpoints[2].address`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub path: String,
    pub message: String,
}

impl fmt::Display for FieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Why a configuration was rejected.
#[derive(Debug)]
pub enum ConfigError {
    /// The file could not be read.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The input is larger than [`limits::MAX_FILE_BYTES`].
    TooLarge { path: Option<PathBuf> },
    /// The input is not well-formed JSON or TOML.
    Syntax { format: Format, message: String },
    /// A value has the wrong type, a field is unknown, or a duration does
    /// not parse.
    Decode { format: Format, error: FieldError },
    /// The document is well-formed but breaks the rules. At most
    /// [`limits::MAX_ERRORS`] problems are listed; `more` counts the rest.
    Invalid {
        errors: Vec<FieldError>,
        more: usize,
    },
}

impl ConfigError {
    /// The problems found by validation, if that is how it failed.
    pub fn field_errors(&self) -> &[FieldError] {
        match self {
            ConfigError::Invalid { errors, .. } => errors,
            ConfigError::Decode { error, .. } => std::slice::from_ref(error),
            _ => &[],
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io { path, source } => write!(f, "reading {}: {source}", path.display()),
            ConfigError::TooLarge { path: Some(path) } => write!(
                f,
                "{} is larger than {} bytes",
                path.display(),
                limits::MAX_FILE_BYTES
            ),
            ConfigError::TooLarge { path: None } => write!(
                f,
                "configuration is larger than {} bytes",
                limits::MAX_FILE_BYTES
            ),
            ConfigError::Syntax { format, message } => {
                write!(f, "invalid {}: {message}", format.name())
            }
            ConfigError::Decode { format, error } => {
                write!(f, "invalid {}: {error}", format.name())
            }
            ConfigError::Invalid { errors, more } => {
                f.write_str("invalid configuration:")?;
                for e in errors {
                    write!(f, "\n{e}")?;
                }
                if *more > 0 {
                    write!(f, "\n({more} more errors)")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl Config {
    /// Parses and validates a configuration in the given syntax.
    pub fn parse(data: &[u8], format: Format) -> Result<Config, ConfigError> {
        if data.len() > limits::MAX_FILE_BYTES {
            return Err(ConfigError::TooLarge { path: None });
        }
        let node = match format {
            Format::Json => node::parse_json(data),
            Format::Toml => node::parse_toml(data),
        }
        .map_err(|message| ConfigError::Syntax { format, message })?;
        let raw = raw::decode(&node).map_err(|e| ConfigError::Decode {
            format,
            error: FieldError {
                path: e.path,
                message: e.message,
            },
        })?;
        validate::validate(&raw)
    }

    /// Parses and validates a JSON configuration, as steer did.
    pub fn from_json(data: &[u8]) -> Result<Config, ConfigError> {
        Config::parse(data, Format::Json)
    }

    /// Parses and validates a TOML configuration.
    pub fn from_toml(data: &[u8]) -> Result<Config, ConfigError> {
        Config::parse(data, Format::Toml)
    }

    /// Reads, parses and validates the file at `path`, choosing the syntax
    /// by its extension. Relative certificate and key paths are resolved
    /// against the directory that holds the file, as `filepath.Join` does.
    pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        use std::io::Read;

        let path = path.as_ref();
        let io = |source| ConfigError::Io {
            path: path.to_owned(),
            source,
        };
        let file = std::fs::File::open(path).map_err(io)?;
        let mut data = Vec::new();
        file.take(limits::MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut data)
            .map_err(io)?;
        if data.len() > limits::MAX_FILE_BYTES {
            return Err(ConfigError::TooLarge {
                path: Some(path.to_owned()),
            });
        }
        let mut cfg = Config::parse(&data, Format::from_path(path))?;
        if let Some(tls) = &mut cfg.tls {
            let dir = path
                .to_str()
                .map(go::dir)
                .unwrap_or_else(|| path.parent().unwrap_or(Path::new("")).display().to_string());
            for cert in &mut tls.certificates {
                for file in [&mut cert.cert_file, &mut cert.key_file] {
                    if !file.starts_with('/') {
                        *file = go::join(&dir, file);
                    }
                }
            }
        }
        Ok(cfg)
    }
}
