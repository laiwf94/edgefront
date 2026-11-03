//! steer's validation rules, applied to the raw schema: defaults, ranges,
//! address and name syntax, and cross references. Every problem is
//! collected (up to [`limits::MAX_ERRORS`]) rather than stopping at the
//! first.

use std::collections::HashSet;
use std::time::Duration;

use super::raw::{self, RawCluster, RawConfig, RawTls};
use super::{
    Budget, Certificate, CipherSuite, Cluster, Config, ConfigError, Endpoint, FieldError,
    HealthCheck, IpPrefix, OutlierDetection, Policy, Retry, Route, Tls, TlsVersion, defaults, go,
    limits,
};

const MS: i64 = 1_000_000;
const SECOND: i64 = 1_000 * MS;
const MINUTE: i64 = 60 * SECOND;
const HOUR: i64 = 60 * MINUTE;

/// The secure suites Go's `crypto/tls` offers, in its order, and whether
/// each one is TLS 1.3 only.
const SECURE_SUITES: &[(CipherSuite, bool)] = &[
    (suite(0x1301, "TLS_AES_128_GCM_SHA256"), true),
    (suite(0x1302, "TLS_AES_256_GCM_SHA384"), true),
    (suite(0x1303, "TLS_CHACHA20_POLY1305_SHA256"), true),
    (suite(0xc009, "TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA"), false),
    (suite(0xc00a, "TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA"), false),
    (suite(0xc013, "TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA"), false),
    (suite(0xc014, "TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA"), false),
    (
        suite(0xc02b, "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256"),
        false,
    ),
    (
        suite(0xc02c, "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384"),
        false,
    ),
    (
        suite(0xc02f, "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"),
        false,
    ),
    (
        suite(0xc030, "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384"),
        false,
    ),
    (
        suite(0xcca8, "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256"),
        false,
    ),
    (
        suite(0xcca9, "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256"),
        false,
    ),
];

const fn suite(id: u16, name: &'static str) -> CipherSuite {
    CipherSuite { id, name }
}

/// The `modern` policy: ECDHE with AES-GCM or ChaCha20-Poly1305 only.
const MODERN: &[u16] = &[0xc02b, 0xc02f, 0xc02c, 0xc030, 0xcca9, 0xcca8];
/// The `compatible` policy: modern plus ECDHE with AES-CBC.
const COMPATIBLE: &[u16] = &[
    0xc02b, 0xc02f, 0xc02c, 0xc030, 0xcca9, 0xcca8, 0xc009, 0xc013, 0xc00a, 0xc014,
];

fn suite_by_id(id: u16) -> CipherSuite {
    SECURE_SUITES
        .iter()
        .find(|(s, _)| s.id == id)
        .map(|&(s, _)| s)
        .expect("policies only name secure suites")
}

pub(crate) fn validate(raw: &RawConfig) -> Result<Config, ConfigError> {
    let mut v = Validator::default();
    let cfg = v.config(raw);
    if v.errors.is_empty() {
        Ok(cfg)
    } else {
        Err(ConfigError::Invalid {
            errors: v.errors,
            more: v.dropped,
        })
    }
}

#[derive(Default)]
struct Validator {
    errors: Vec<FieldError>,
    dropped: usize,
}

/// Converts a value already checked to be in range; out-of-range values
/// only occur alongside a recorded error, when the config is discarded.
fn narrow<T: TryFrom<i64> + Default>(v: i64) -> T {
    T::try_from(v).unwrap_or_default()
}

fn to_std(nanos: i64) -> Duration {
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(0))
}

impl Validator {
    fn add(&mut self, path: impl Into<String>, message: impl Into<String>) {
        if self.errors.len() >= limits::MAX_ERRORS {
            self.dropped += 1;
            return;
        }
        self.errors.push(FieldError {
            path: path.into(),
            message: message.into(),
        });
    }

    fn int_in(&mut self, path: &str, v: Option<i64>, def: i64, lo: i64, hi: i64) -> i64 {
        let Some(v) = v else { return def };
        if v < lo || v > hi {
            self.add(path, format!("must be between {lo} and {hi}"));
        }
        v
    }

    fn dur_in(&mut self, path: &str, v: Option<raw::Duration>, def: i64, lo: i64, hi: i64) -> i64 {
        let Some(raw::Duration(v)) = v else {
            return def;
        };
        if v < lo || v > hi {
            self.add(
                path,
                format!(
                    "must be between {} and {}",
                    go::format_duration(lo),
                    go::format_duration(hi)
                ),
            );
        }
        v
    }

    fn config(&mut self, raw: &RawConfig) -> Config {
        let mut cfg = Config {
            listen: raw.listen.clone(),
            admin_listen: raw.admin_listen.clone(),
            trusted_proxies: Vec::new(),
            tls: None,
            routes: Vec::new(),
            clusters: Vec::new(),
        };
        if cfg.listen.is_empty() {
            self.add("listen", "is required, e.g. \":8080\"");
        } else if let Err(e) = check_listen(&cfg.listen) {
            self.add("listen", e);
        }
        if cfg.admin_listen.is_empty() {
            cfg.admin_listen = defaults::ADMIN_LISTEN.to_owned();
        } else if let Err(e) = check_listen(&cfg.admin_listen) {
            self.add("admin_listen", e);
        }
        if !cfg.listen.is_empty() && cfg.listen == cfg.admin_listen && !cfg.listen.ends_with(":0") {
            self.add("admin_listen", "must differ from listen");
        }

        let proxies = raw.trusted_proxies.as_deref().unwrap_or_default();
        if proxies.len() > limits::MAX_TRUSTED_PROXIES {
            self.add(
                "trusted_proxies",
                format!("at most {} entries", limits::MAX_TRUSTED_PROXIES),
            );
        }
        for (i, s) in proxies.iter().enumerate() {
            match parse_trusted_prefix(s) {
                Ok(p) => cfg.trusted_proxies.push(p),
                Err(e) => self.add(format!("trusted_proxies[{i}]"), e),
            }
        }
        if let Some(t) = &raw.tls {
            cfg.tls = Some(self.tls("tls", t));
        }

        let clusters = raw.clusters.as_deref().unwrap_or_default();
        if clusters.is_empty() {
            self.add("clusters", "at least one cluster is required");
        }
        if clusters.len() > limits::MAX_CLUSTERS {
            self.add(
                "clusters",
                format!("at most {} clusters", limits::MAX_CLUSTERS),
            );
        }
        let mut names = HashSet::new();
        for (i, rc) in clusters.iter().enumerate() {
            let path = format!("clusters[{i}]");
            let c = self.cluster(&path, rc);
            if !names.insert(c.name.clone()) {
                self.add(
                    format!("{path}.name"),
                    format!("duplicate cluster {}", go::quote(&c.name)),
                );
            }
            cfg.clusters.push(c);
        }

        let routes = raw.routes.as_deref().unwrap_or_default();
        if routes.is_empty() {
            self.add("routes", "at least one route is required");
        }
        if routes.len() > limits::MAX_ROUTES {
            self.add("routes", format!("at most {} routes", limits::MAX_ROUTES));
        }
        let mut seen = HashSet::new();
        for (i, rr) in routes.iter().enumerate() {
            let path = format!("routes[{i}]");
            let mut r = Route {
                host: go::to_lower(&rr.host),
                path_prefix: rr.path_prefix.clone(),
                cluster: rr.cluster.clone(),
            };
            if r.host.is_empty() {
                r.host = "*".to_owned();
            }
            if let Err(e) = check_host_pattern(&r.host) {
                self.add(format!("{path}.host"), e);
            }
            if r.path_prefix.is_empty() {
                r.path_prefix = "/".to_owned();
            }
            if let Err(e) = check_path_prefix(&r.path_prefix) {
                self.add(format!("{path}.path_prefix"), e);
            }
            if !names.contains(&r.cluster) {
                self.add(
                    format!("{path}.cluster"),
                    format!("unknown cluster {}", go::quote(&r.cluster)),
                );
            }
            if !seen.insert((r.host.clone(), r.path_prefix.clone())) {
                self.add(
                    path,
                    format!(
                        "duplicate route for host {} and path_prefix {}",
                        go::quote(&r.host),
                        go::quote(&r.path_prefix)
                    ),
                );
            }
            cfg.routes.push(r);
        }
        cfg
    }

    fn tls(&mut self, path: &str, rt: &RawTls) -> Tls {
        let min_version = match rt.min_version.as_str() {
            "" | "1.2" => TlsVersion::Tls12,
            "1.3" => TlsVersion::Tls13,
            _ => {
                self.add(format!("{path}.min_version"), r#"must be "1.2" or "1.3""#);
                TlsVersion::Tls12
            }
        };
        let mut cipher_suites = match (&rt.cipher_suites, rt.cipher_policy.as_str()) {
            (Some(_), policy) if !policy.is_empty() => {
                self.add(
                    format!("{path}.cipher_suites"),
                    "cannot be combined with cipher_policy",
                );
                Some(Vec::new())
            }
            (Some(names), _) => Some(self.cipher_suites(&format!("{path}.cipher_suites"), names)),
            (None, "" | "modern") => Some(MODERN.iter().map(|&id| suite_by_id(id)).collect()),
            (None, "compatible") => Some(COMPATIBLE.iter().map(|&id| suite_by_id(id)).collect()),
            (None, _) => {
                self.add(
                    format!("{path}.cipher_policy"),
                    r#"must be "modern" or "compatible""#,
                );
                Some(Vec::new())
            }
        };
        if min_version == TlsVersion::Tls13 {
            if !rt.cipher_policy.is_empty() || rt.cipher_suites.is_some() {
                self.add(
                    format!("{path}.min_version"),
                    "TLS 1.3 suites are not configurable; drop cipher_policy and cipher_suites",
                );
            }
            cipher_suites = None;
        }
        let handshake_timeout = self.dur_in(
            &format!("{path}.handshake_timeout"),
            rt.handshake_timeout,
            nanos(defaults::HANDSHAKE_TIMEOUT),
            nanos(limits::MIN_HANDSHAKE_TIMEOUT),
            nanos(limits::MAX_HANDSHAKE_TIMEOUT),
        );
        let max_concurrent_handshakes = self.int_in(
            &format!("{path}.max_concurrent_handshakes"),
            rt.max_concurrent_handshakes,
            defaults::MAX_CONCURRENT_HANDSHAKES.into(),
            1,
            limits::MAX_CONCURRENT_HANDSHAKE_CAP.into(),
        );

        let certs = rt.certificates.as_deref().unwrap_or_default();
        if certs.is_empty() {
            self.add(
                format!("{path}.certificates"),
                "at least one certificate is required",
            );
        }
        if certs.len() > limits::MAX_CERTIFICATES {
            self.add(
                format!("{path}.certificates"),
                format!("at most {} certificates", limits::MAX_CERTIFICATES),
            );
        }
        let mut certificates = Vec::new();
        for (i, rc) in certs.iter().enumerate() {
            let cp = format!("{path}.certificates[{i}]");
            if rc.cert_file.is_empty() {
                self.add(format!("{cp}.cert_file"), "is required");
            }
            if rc.key_file.is_empty() {
                self.add(format!("{cp}.key_file"), "is required");
            }
            let mut server_names = None;
            if let Some(names) = &rc.server_names {
                if names.is_empty() {
                    self.add(
                        format!("{cp}.server_names"),
                        "must not be empty; omit it to use the certificate's names",
                    );
                }
                if names.len() > limits::MAX_SERVER_NAMES_PER_CERT {
                    self.add(
                        format!("{cp}.server_names"),
                        format!("at most {} names", limits::MAX_SERVER_NAMES_PER_CERT),
                    );
                }
                let mut kept = Vec::new();
                for (j, n) in names.iter().enumerate() {
                    let n = go::to_lower(n);
                    if n == "*" || check_host_pattern(&n).is_err() {
                        self.add(
                            format!("{cp}.server_names[{j}]"),
                            format!("{} must be a host name or *.suffix", go::quote(&n)),
                        );
                        continue;
                    }
                    kept.push(n);
                }
                server_names = Some(kept);
            }
            certificates.push(Certificate {
                cert_file: rc.cert_file.clone(),
                key_file: rc.key_file.clone(),
                server_names,
            });
        }
        Tls {
            certificates,
            min_version,
            cipher_suites,
            handshake_timeout: to_std(handshake_timeout),
            max_concurrent_handshakes: narrow(max_concurrent_handshakes),
            strict_sni: rt.strict_sni,
        }
    }

    fn cipher_suites(&mut self, path: &str, names: &[String]) -> Vec<CipherSuite> {
        if names.is_empty() {
            self.add(path, "must not be empty");
            return Vec::new();
        }
        let mut out: Vec<CipherSuite> = Vec::new();
        for (i, n) in names.iter().enumerate() {
            let at = format!("{path}[{i}]");
            match SECURE_SUITES.iter().find(|(s, _)| s.name == n) {
                None => self.add(
                    at,
                    format!("{} is not a supported secure cipher suite", go::quote(n)),
                ),
                Some((_, true)) => self.add(
                    at,
                    format!(
                        "{} is a TLS 1.3 suite; those are always enabled",
                        go::quote(n)
                    ),
                ),
                Some((s, false)) if out.contains(s) => {
                    self.add(at, format!("duplicate cipher suite {}", go::quote(n)));
                }
                Some(&(s, false)) => out.push(s),
            }
        }
        out
    }

    fn cluster(&mut self, path: &str, rc: &RawCluster) -> Cluster {
        if !valid_name(&rc.name) {
            self.add(
                format!("{path}.name"),
                "must be 1 to 64 characters from A-Z a-z 0-9 . _ -",
            );
        }
        let policy = match rc.policy.as_str() {
            "" | "round_robin" => Policy::RoundRobin,
            "least_loaded" => Policy::LeastLoaded,
            "maglev" => Policy::Maglev,
            _ => {
                self.add(
                    format!("{path}.policy"),
                    "must be round_robin, least_loaded or maglev",
                );
                Policy::RoundRobin
            }
        };
        let mut hash_header = None;
        if policy == Policy::Maglev {
            if valid_token(&rc.hash_header) {
                hash_header = Some(go::canonical_header_key(&rc.hash_header));
            } else {
                self.add(
                    format!("{path}.hash_header"),
                    "is required for maglev and must be a valid header name",
                );
            }
        } else if !rc.hash_header.is_empty() {
            self.add(
                format!("{path}.hash_header"),
                "is only used with the maglev policy",
            );
        }

        let raw_endpoints = rc.endpoints.as_deref().unwrap_or_default();
        if raw_endpoints.is_empty() {
            self.add(
                format!("{path}.endpoints"),
                "at least one endpoint is required",
            );
        }
        if raw_endpoints.len() > limits::MAX_ENDPOINTS {
            self.add(
                format!("{path}.endpoints"),
                format!("at most {} endpoints", limits::MAX_ENDPOINTS),
            );
        }
        let mut addrs = HashSet::new();
        let mut endpoints = Vec::new();
        for (i, re) in raw_endpoints.iter().enumerate() {
            let ep = format!("{path}.endpoints[{i}]");
            if let Err(e) = check_endpoint_address(&re.address) {
                self.add(format!("{ep}.address"), e);
            }
            if !addrs.insert(re.address.as_str()) {
                self.add(
                    format!("{ep}.address"),
                    format!("duplicate address {}", go::quote(&re.address)),
                );
            }
            let weight = self.int_in(
                &format!("{ep}.weight"),
                re.weight,
                1,
                1,
                limits::MAX_WEIGHT.into(),
            );
            let priority = self.int_in(
                &format!("{ep}.priority"),
                re.priority,
                0,
                0,
                limits::MAX_PRIORITY.into(),
            );
            endpoints.push(Endpoint {
                address: re.address.clone(),
                weight: narrow(weight),
                priority: narrow(priority),
            });
        }

        let connect_timeout = self.dur_in(
            &format!("{path}.connect_timeout"),
            rc.connect_timeout,
            nanos(defaults::CONNECT_TIMEOUT),
            MS,
            MINUTE,
        );
        let timeout = self.dur_in(
            &format!("{path}.timeout"),
            rc.timeout,
            nanos(defaults::TIMEOUT),
            0,
            HOUR,
        );
        let max_requests = self.int_in(
            &format!("{path}.max_requests"),
            rc.max_requests,
            defaults::MAX_REQUESTS.into(),
            1,
            1_000_000,
        );
        let panic_threshold = self.int_in(
            &format!("{path}.panic_threshold_percent"),
            rc.panic_threshold_percent,
            defaults::PANIC_THRESHOLD_PERCENT.into(),
            0,
            100,
        );
        let health_check = rc
            .health_check
            .as_ref()
            .map(|h| self.health_check(&format!("{path}.health_check"), h));
        let outlier_detection = rc
            .outlier_detection
            .as_ref()
            .map(|o| self.outlier(&format!("{path}.outlier_detection"), o));
        let retry = rc
            .retry
            .as_ref()
            .map(|r| self.retry(&format!("{path}.retry"), r, timeout));
        Cluster {
            name: rc.name.clone(),
            policy,
            hash_header,
            endpoints,
            connect_timeout: to_std(connect_timeout),
            timeout: to_std(timeout),
            max_requests: narrow(max_requests),
            panic_threshold_percent: narrow(panic_threshold),
            health_check,
            outlier_detection,
            retry,
        }
    }

    fn health_check(&mut self, hp: &str, h: &raw::RawHealth) -> HealthCheck {
        if let Err(e) = check_probe_path(&h.path) {
            self.add(format!("{hp}.path"), e);
        }
        if !h.host.is_empty()
            && check_endpoint_address(&h.host).is_err()
            && check_hostname(&h.host).is_err()
        {
            self.add(format!("{hp}.host"), "must be a host name or host:port");
        }
        let interval = self.dur_in(
            &format!("{hp}.interval"),
            h.interval,
            nanos(defaults::HEALTH_INTERVAL),
            10 * MS,
            HOUR,
        );
        let jitter = self.dur_in(&format!("{hp}.jitter"), h.jitter, 0, 0, HOUR);
        let timeout = self.dur_in(
            &format!("{hp}.timeout"),
            h.timeout,
            nanos(defaults::HEALTH_TIMEOUT).min(interval),
            MS,
            HOUR,
        );
        if timeout > interval {
            self.add(format!("{hp}.timeout"), "must not exceed interval");
        }
        if jitter > interval {
            self.add(format!("{hp}.jitter"), "must not exceed interval");
        }
        let healthy = self.int_in(
            &format!("{hp}.healthy_threshold"),
            h.healthy_threshold,
            defaults::HEALTHY_AFTER.into(),
            1,
            100,
        );
        let unhealthy = self.int_in(
            &format!("{hp}.unhealthy_threshold"),
            h.unhealthy_threshold,
            defaults::UNHEALTHY_AFTER.into(),
            1,
            100,
        );
        HealthCheck {
            path: h.path.clone(),
            host: h.host.clone(),
            interval: to_std(interval),
            jitter: to_std(jitter),
            timeout: to_std(timeout),
            healthy_threshold: narrow(healthy),
            unhealthy_threshold: narrow(unhealthy),
        }
    }

    fn outlier(&mut self, op: &str, o: &raw::RawOutlier) -> OutlierDetection {
        let failures = self.int_in(
            &format!("{op}.consecutive_gateway_failures"),
            o.consecutive_gateway_failures,
            defaults::CONSECUTIVE_GATEWAY_FAILURES.into(),
            0,
            1000,
        );
        let interval = self.dur_in(
            &format!("{op}.interval"),
            o.interval,
            nanos(defaults::OUTLIER_INTERVAL),
            10 * MS,
            HOUR,
        );
        let base = self.dur_in(
            &format!("{op}.base_ejection_time"),
            o.base_ejection_time,
            nanos(defaults::BASE_EJECTION_TIME),
            MS,
            HOUR,
        );
        let max = self.dur_in(
            &format!("{op}.max_ejection_time"),
            o.max_ejection_time,
            nanos(defaults::MAX_EJECTION_TIME).max(base),
            MS,
            24 * HOUR,
        );
        if max < base {
            self.add(
                format!("{op}.max_ejection_time"),
                "must not be less than base_ejection_time",
            );
        }
        let max_percent = self.int_in(
            &format!("{op}.max_ejection_percent"),
            o.max_ejection_percent,
            defaults::MAX_EJECTION_PERCENT.into(),
            0,
            100,
        );
        let min_hosts = self.int_in(
            &format!("{op}.success_rate_minimum_hosts"),
            o.success_rate_minimum_hosts,
            defaults::SUCCESS_RATE_MINIMUM_HOSTS.into(),
            0,
            limits::MAX_ENDPOINTS as i64,
        );
        let volume = self.int_in(
            &format!("{op}.success_rate_request_volume"),
            o.success_rate_request_volume,
            defaults::SUCCESS_RATE_REQUEST_VOLUME.into(),
            1,
            1_000_000,
        );
        let mut factor = defaults::SUCCESS_RATE_STDEV_FACTOR;
        if let Some(f) = o.success_rate_stdev_factor {
            if !(f > 0.0 && f <= 10.0) {
                self.add(
                    format!("{op}.success_rate_stdev_factor"),
                    "must be greater than 0 and at most 10",
                );
            }
            factor = f;
        }
        OutlierDetection {
            consecutive_gateway_failures: narrow(failures),
            interval: to_std(interval),
            base_ejection_time: to_std(base),
            max_ejection_time: to_std(max),
            max_ejection_percent: narrow(max_percent),
            success_rate_minimum_hosts: narrow(min_hosts),
            success_rate_request_volume: narrow(volume),
            success_rate_stdev_factor: factor,
        }
    }

    fn retry(&mut self, rp: &str, r: &raw::RawRetry, cluster_timeout: i64) -> Retry {
        let max_retries = self.int_in(
            &format!("{rp}.max_retries"),
            r.max_retries,
            defaults::MAX_RETRIES.into(),
            0,
            limits::MAX_RETRIES.into(),
        );
        let retry_on = match &r.retry_on {
            None => defaults::RETRY_ON.to_vec(),
            Some(statuses) => {
                let mut seen = HashSet::new();
                let mut out = Vec::new();
                for (i, &s) in statuses.iter().enumerate() {
                    if !(500..=599).contains(&s) {
                        self.add(format!("{rp}.retry_on[{i}]"), "must be a 5xx status");
                    }
                    if !seen.insert(s) {
                        self.add(
                            format!("{rp}.retry_on[{i}]"),
                            format!("duplicate status {s}"),
                        );
                    }
                    out.push(narrow(s));
                }
                out
            }
        };
        let per_try = self.dur_in(
            &format!("{rp}.per_try_timeout"),
            r.per_try_timeout,
            0,
            0,
            HOUR,
        );
        if cluster_timeout > 0 && per_try > cluster_timeout {
            self.add(
                format!("{rp}.per_try_timeout"),
                "must not exceed the cluster timeout",
            );
        }
        let backoff_base = self.dur_in(
            &format!("{rp}.backoff_base"),
            r.backoff_base,
            nanos(defaults::BACKOFF_BASE),
            MS,
            MINUTE,
        );
        let backoff_max = self.dur_in(
            &format!("{rp}.backoff_max"),
            r.backoff_max,
            nanos(defaults::BACKOFF_MAX).max(backoff_base),
            MS,
            MINUTE,
        );
        if backoff_max < backoff_base {
            self.add(
                format!("{rp}.backoff_max"),
                "must not be less than backoff_base",
            );
        }
        let mut max_body_bytes = defaults::RETRY_BODY_BYTES as i64;
        if let Some(b) = r.max_body_bytes {
            if b < 0 || b > limits::MAX_RETRY_BODY_BYTES as i64 {
                self.add(
                    format!("{rp}.max_body_bytes"),
                    format!("must be between 0 and {}", limits::MAX_RETRY_BODY_BYTES),
                );
            }
            max_body_bytes = b;
        }
        let mut budget = Budget {
            ratio: defaults::BUDGET_RATIO,
            min_per_second: defaults::BUDGET_MIN_PER_SECOND,
            window: defaults::BUDGET_WINDOW,
        };
        if let Some(b) = &r.budget {
            let bp = format!("{rp}.budget");
            if let Some(ratio) = b.ratio {
                if !(0.0..=1.0).contains(&ratio) {
                    self.add(format!("{bp}.ratio"), "must be between 0 and 1");
                }
                budget.ratio = ratio;
            }
            let min = self.int_in(
                &format!("{bp}.min_per_second"),
                b.min_per_second,
                defaults::BUDGET_MIN_PER_SECOND.into(),
                0,
                100_000,
            );
            budget.min_per_second = narrow(min);
            let window = self.dur_in(
                &format!("{bp}.window"),
                b.window,
                nanos(defaults::BUDGET_WINDOW),
                SECOND,
                MINUTE,
            );
            if window % SECOND != 0 {
                self.add(format!("{bp}.window"), "must be a whole number of seconds");
            }
            budget.window = to_std(window);
        }
        Retry {
            max_retries: narrow(max_retries),
            retry_on,
            per_try_timeout: to_std(per_try),
            backoff_base: to_std(backoff_base),
            backoff_max: to_std(backoff_max),
            max_body_bytes: narrow(max_body_bytes),
            budget,
        }
    }
}

fn nanos(d: Duration) -> i64 {
    i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
}

/// 1 to 64 characters from `A-Z a-z 0-9 . _ -`.
fn valid_name(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// An RFC 9110 token of at most 64 bytes.
fn valid_token(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
}

fn check_listen(addr: &str) -> Result<(), String> {
    let (host, port) =
        go::split_host_port(addr).map_err(|_| format!("{} is not host:port", go::quote(addr)))?;
    check_port(port, true)?;
    if host.is_empty() || host == "localhost" || go::parse_addr(host).is_some() {
        return Ok(());
    }
    Err(format!(
        "listen host {} must be an IP address, localhost or empty",
        go::quote(host)
    ))
}

/// A canonical decimal port: no sign, no leading zeros, at most 65535.
fn check_port(port: &str, allow_zero: bool) -> Result<(), String> {
    let canonical = !port.is_empty()
        && port.bytes().all(|c| c.is_ascii_digit())
        && (port == "0" || !port.starts_with('0'));
    match port.parse::<u32>() {
        Ok(n) if canonical && n <= 65535 && (n != 0 || allow_zero) => Ok(()),
        _ => Err(format!(
            "port {} must be a number between 1 and 65535",
            go::quote(port)
        )),
    }
}

fn check_endpoint_address(addr: &str) -> Result<(), String> {
    let (host, port) =
        go::split_host_port(addr).map_err(|_| format!("{} is not host:port", go::quote(addr)))?;
    check_port(port, false)?;
    if go::parse_addr(host).is_some() {
        return Ok(());
    }
    check_hostname(host)
}

/// A DNS name made of letters, digits and hyphens.
fn check_hostname(h: &str) -> Result<(), String> {
    if h.is_empty() || h.len() > 253 {
        return Err(format!("host {} must be 1 to 253 characters", go::quote(h)));
    }
    let valid = h.split('.').all(|label| {
        (1..=63).contains(&label.len())
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
    });
    if valid {
        Ok(())
    } else {
        Err(format!("host {} is not a valid DNS name", go::quote(h)))
    }
}

fn check_host_pattern(h: &str) -> Result<(), String> {
    if h == "*" {
        return Ok(());
    }
    if let Some(rest) = h.strip_prefix("*.") {
        return check_hostname(rest)
            .map_err(|_| format!("{} must be *, a host name, or *.suffix", go::quote(h)));
    }
    check_hostname(h).map_err(|_| {
        format!(
            "{} must be *, a host name, or *.suffix (no port)",
            go::quote(h)
        )
    })
}

/// An absolute path without empty, `.` or `..` segments, so the prefix can
/// be compared with cleaned request paths.
fn check_path_prefix(p: &str) -> Result<(), String> {
    if p.len() > limits::MAX_PATH_PREFIX_LEN {
        return Err(format!(
            "must be at most {} characters",
            limits::MAX_PATH_PREFIX_LEN
        ));
    }
    let Some(rest) = p.strip_prefix('/') else {
        return Err(r#"must start with "/""#.to_owned());
    };
    if p.bytes()
        .any(|c| c <= b' ' || matches!(c, 0x7f | b'?' | b'#' | b'%' | b'\\'))
    {
        return Err(
            "must not contain spaces, control characters, ?, #, % or backslashes".to_owned(),
        );
    }
    let segs: Vec<&str> = rest.split('/').collect();
    let bad = segs
        .iter()
        .enumerate()
        .any(|(i, &s)| s == "." || s == ".." || (s.is_empty() && i != segs.len() - 1));
    if bad {
        return Err(r#"must not contain empty, "." or ".." segments"#.to_owned());
    }
    Ok(())
}

fn check_probe_path(p: &str) -> Result<(), String> {
    if !p.starts_with('/') || p.len() > limits::MAX_PATH_PREFIX_LEN {
        return Err(format!(
            r#"must start with "/" and be at most {} characters"#,
            limits::MAX_PATH_PREFIX_LEN
        ));
    }
    if p.bytes().any(|c| c <= b' ' || c == 0x7f || c == b'#') {
        return Err("must not contain spaces, control characters or #".to_owned());
    }
    Ok(())
}

/// A CIDR without host bits, or a single address without a zone.
pub(crate) fn parse_trusted_prefix(s: &str) -> Result<IpPrefix, String> {
    if let Some((addr, len)) = go::parse_prefix(s) {
        let masked = go::mask(addr, len);
        if masked != addr {
            let want = IpPrefix { addr: masked, len };
            return Err(format!(
                "{} has host bits set; did you mean {want}?",
                go::quote(s)
            ));
        }
        return Ok(IpPrefix { addr, len });
    }
    match go::parse_addr(s) {
        Some(a) if a.zone.is_none() => Ok(IpPrefix {
            addr: a.ip,
            len: if a.ip.is_ipv4() { 32 } else { 128 },
        }),
        _ => Err(format!("{} is not an IP address or CIDR", go::quote(s))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_match_steer() {
        assert!(!valid_token(""));
        assert!(!valid_token(&"a".repeat(65)));
        assert!(valid_token("X-Request_Id.2~"));
        assert!(!valid_token("bad header"));
        assert!(check_hostname(&"a".repeat(64)).is_err());
        assert!(check_hostname("a..b").is_err());
        assert!(check_hostname("ok-1.b2").is_ok());
        assert!(check_hostname(&format!("{}.a", "a.".repeat(126))).is_err());
        assert!(check_listen("localhost:0").is_ok());
        assert!(check_listen("[::1]:80").is_ok());
        assert!(check_listen("[fe80::1%eth0]:80").is_ok());
        assert!(check_listen("example.com:80").is_err());
        assert!(parse_trusted_prefix("fe80::1%eth0").is_err());
        assert!(valid_name("a.b_c-D9"));
        assert!(!valid_name(""));
        assert!(!valid_name(&"a".repeat(65)));
    }

    #[test]
    fn ports_are_canonical_decimal() {
        for ok in ["1", "80", "65535"] {
            assert!(check_port(ok, false).is_ok(), "{ok}");
        }
        assert!(check_port("0", true).is_ok());
        for bad in [
            "0",
            "",
            "01",
            "+80",
            "-1",
            "-0",
            "65536",
            "99999999999",
            "8a",
            " 80",
        ] {
            assert!(check_port(bad, false).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn trusted_proxies_like_go() {
        let p = parse_trusted_prefix("192.168.1.7").unwrap();
        assert_eq!(p.to_string(), "192.168.1.7/32");
        assert_eq!(
            parse_trusted_prefix("::1.2.3.4").unwrap().to_string(),
            "::102:304/128"
        );
        assert_eq!(
            parse_trusted_prefix("::ffff:1.2.3.0/120")
                .unwrap()
                .to_string(),
            "::ffff:1.2.3.0/120"
        );
        let err = parse_trusted_prefix("10.0.0.1/8").unwrap_err();
        assert!(err.contains("did you mean 10.0.0.0/8?"), "{err}");
        assert!(
            parse_trusted_prefix("nope")
                .unwrap_err()
                .contains("not an IP address")
        );
    }

    #[test]
    fn policies_name_only_secure_tls12_suites() {
        for &id in MODERN.iter().chain(COMPATIBLE) {
            let s = suite_by_id(id);
            assert!(SECURE_SUITES.contains(&(s, false)), "{}", s.name);
        }
        assert!(
            MODERN
                .iter()
                .all(|&id| !suite_by_id(id).name.contains("CBC"))
        );
    }
}
