//! The config loader against steer: its Go test suite ported case for case,
//! every example file in legacy/steer/examples (in JSON and in TOML), a
//! corpus of edge cases with steer's own verdicts, and properties over
//! generated configs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use edgefront_core::config::{Config, ConfigError, Format, Policy, TlsVersion, defaults, limits};
use proptest::prelude::*;

const FULL: &str = r#"{
  "listen": ":8080",
  "admin_listen": "127.0.0.1:9901",
  "trusted_proxies": ["10.0.0.0/8", "192.168.1.7"],
  "routes": [
    {"host": "API.example.com", "path_prefix": "/v1/", "cluster": "api"},
    {"cluster": "web"}
  ],
  "clusters": [
    {
      "name": "api",
      "policy": "maglev",
      "hash_header": "x-user-id",
      "endpoints": [
        {"address": "10.0.1.1:9000", "weight": 3},
        {"address": "api-backup.internal:9000", "priority": 1}
      ],
      "connect_timeout": "250ms",
      "timeout": "0s",
      "max_requests": 50,
      "panic_threshold_percent": 0,
      "health_check": {"path": "/healthz", "interval": "2s", "jitter": "200ms", "timeout": "500ms",
                       "healthy_threshold": 1, "unhealthy_threshold": 3, "host": "health.internal"},
      "outlier_detection": {"consecutive_gateway_failures": 0, "interval": "1s", "base_ejection_time": "5s",
                            "max_ejection_percent": 50, "success_rate_minimum_hosts": 2,
                            "success_rate_request_volume": 20, "success_rate_stdev_factor": 1.5},
      "retry": {"max_retries": 3, "retry_on": [503], "per_try_timeout": "300ms", "backoff_base": "10ms",
                "backoff_max": "100ms", "max_body_bytes": 1024,
                "budget": {"ratio": 0.1, "min_per_second": 2, "window": "5s"}}
    },
    {"name": "web", "endpoints": [{"address": "[::1]:8081"}]}
  ]
}"#;

const MINIMAL: &str = r#"{"listen":":80","routes":[{"cluster":"a"}],"clusters":[{"name":"a","endpoints":[{"address":"h:1"}]}]}"#;

fn parse(s: &str) -> Result<Config, ConfigError> {
    Config::from_json(s.as_bytes())
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn steer_examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../legacy/steer/examples")
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn parses_the_full_example() {
    let c = parse(FULL).unwrap();
    assert_eq!(c.listen, ":8080");
    assert_eq!(c.trusted_proxies.len(), 2);
    assert_eq!(c.trusted_proxies[1].to_string(), "192.168.1.7/32");
    assert_eq!(c.routes[0].host, "api.example.com");
    assert_eq!(c.routes[1].host, "*");
    assert_eq!(c.routes[1].path_prefix, "/");
    let api = &c.clusters[0];
    assert_eq!(api.policy, Policy::Maglev);
    assert_eq!(api.hash_header.as_deref(), Some("X-User-Id"));
    assert_eq!(api.timeout, Duration::ZERO);
    assert_eq!(api.max_requests, 50);
    assert_eq!(api.panic_threshold_percent, 0);
    assert_eq!(api.connect_timeout, ms(250));
    assert_eq!(api.endpoints[0].weight, 3);
    assert_eq!(api.endpoints[1].priority, 1);
    assert_eq!(api.endpoints[1].weight, 1);
    let h = api.health_check.as_ref().unwrap();
    assert_eq!(h.interval, ms(2000));
    assert_eq!(h.healthy_threshold, 1);
    assert_eq!(h.host, "health.internal");
    let o = api.outlier_detection.as_ref().unwrap();
    assert_eq!(o.consecutive_gateway_failures, 0);
    assert_eq!(o.max_ejection_time, defaults::MAX_EJECTION_TIME);
    assert_eq!(o.success_rate_stdev_factor, 1.5);
    let r = api.retry.as_ref().unwrap();
    assert_eq!(r.max_retries, 3);
    assert_eq!(r.retry_on, [503]);
    assert_eq!(r.max_body_bytes, 1024);
    assert_eq!(r.budget.window, ms(5000));
    assert_eq!(r.budget.ratio, 0.1);
    let web = &c.clusters[1];
    assert_eq!(web.policy, Policy::RoundRobin);
    assert_eq!(web.hash_header, None);
    assert_eq!(web.timeout, defaults::TIMEOUT);
    assert_eq!(web.max_requests, defaults::MAX_REQUESTS);
    assert!(web.health_check.is_none() && web.outlier_detection.is_none() && web.retry.is_none());
    assert_eq!(web.panic_threshold_percent, 50);
}

#[test]
fn applies_defaults() {
    let c = parse(
        r#"{"listen":":80","routes":[{"cluster":"a"}],"clusters":[{"name":"a","endpoints":[{"address":"h:1"}],
            "health_check":{"path":"/"},"outlier_detection":{},"retry":{"budget":{}}}]}"#,
    )
    .unwrap();
    assert_eq!(c.admin_listen, defaults::ADMIN_LISTEN);
    let a = &c.clusters[0];
    let h = a.health_check.as_ref().unwrap();
    assert_eq!(h.interval, defaults::HEALTH_INTERVAL);
    assert_eq!(h.timeout, defaults::HEALTH_TIMEOUT);
    assert_eq!(h.unhealthy_threshold, 2);
    let o = a.outlier_detection.as_ref().unwrap();
    assert_eq!(o.consecutive_gateway_failures, 5);
    assert_eq!(o.max_ejection_percent, 10);
    assert_eq!(o.success_rate_minimum_hosts, 5);
    let r = a.retry.as_ref().unwrap();
    assert_eq!(r.max_retries, 2);
    assert_eq!(r.retry_on, defaults::RETRY_ON);
    assert_eq!(r.backoff_max, defaults::BACKOFF_MAX);
    assert_eq!(r.budget.min_per_second, 10);

    // A short interval lowers the default probe timeout to fit.
    let c = parse(
        r#"{"listen":":80","routes":[{"cluster":"a"}],"clusters":[{"name":"a","endpoints":[{"address":"h:1"}],
            "health_check":{"path":"/","interval":"100ms"}}]}"#,
    )
    .unwrap();
    assert_eq!(
        c.clusters[0].health_check.as_ref().unwrap().timeout,
        ms(100)
    );
    // A long base ejection time raises the default maximum, and a long
    // backoff base raises the default backoff maximum.
    let c = parse(
        r#"{"listen":":80","routes":[{"cluster":"a"}],"clusters":[{"name":"a","endpoints":[{"address":"h:1"}],
            "outlier_detection":{"base_ejection_time":"10m"},"retry":{"backoff_base":"1s"}}]}"#,
    )
    .unwrap();
    let a = &c.clusters[0];
    assert_eq!(
        a.outlier_detection.as_ref().unwrap().max_ejection_time,
        ms(600_000)
    );
    assert_eq!(a.retry.as_ref().unwrap().backoff_max, ms(1000));
}

/// steer's TestErrors: each input fails, with a message that mentions the
/// given text.
#[test]
fn rejects_with_the_field_path() {
    let base = |cluster: &str| {
        format!(r#"{{"listen":":80","routes":[{{"cluster":"a"}}],"clusters":[{cluster}]}}"#)
    };
    let ep = r#""endpoints":[{"address":"h:1"}]"#;
    let with = |extra: &str| base(&format!(r#"{{"name":"a",{ep},{extra}}}"#));
    let route =
        |r: &str| format!(r#"{{"listen":":80","routes":[{r}],"clusters":[{{"name":"a",{ep}}}]}}"#);
    let cases: Vec<(String, &str)> = vec![
        ("not json".into(), "invalid JSON"),
        (r#"{"listen":":80"} {}"#.into(), "trailing data"),
        (r#"{"listen":":80","x":1}"#.into(), "unknown field"),
        ("{}".into(), "listen: is required"),
        (r#"{"listen":"8080"}"#.into(), "not host:port"),
        (r#"{"listen":":99999"}"#.into(), "port"),
        (
            r#"{"listen":"example.com:80"}"#.into(),
            "must be an IP address",
        ),
        (
            r#"{"listen":":80","admin_listen":":80"}"#.into(),
            "must differ",
        ),
        (
            r#"{"listen":":80","admin_listen":"x"}"#.into(),
            "admin_listen",
        ),
        (
            r#"{"listen":":80","trusted_proxies":["10.0.0.1/8"]}"#.into(),
            "host bits set",
        ),
        (
            r#"{"listen":":80","trusted_proxies":["nope"]}"#.into(),
            "not an IP address",
        ),
        (
            r#"{"listen":":80","routes":[]}"#.into(),
            "at least one route",
        ),
        (
            r#"{"listen":":80","clusters":[]}"#.into(),
            "at least one cluster",
        ),
        (with(r#""timeout":5"#), "durations are strings"),
        (with(r#""timeout":"soon""#), "invalid duration"),
        (
            base(&format!(r#"{{"name":"a b",{ep}}}"#)),
            "name: must be 1 to 64",
        ),
        (
            base(&format!(r#"{{"name":"a",{ep}}},{{"name":"a",{ep}}}"#)),
            "duplicate cluster",
        ),
        (
            base(&format!(r#"{{"name":"a","policy":"random",{ep}}}"#)),
            "policy",
        ),
        (
            base(&format!(r#"{{"name":"a","policy":"maglev",{ep}}}"#)),
            "hash_header: is required",
        ),
        (
            base(&format!(
                r#"{{"name":"a","policy":"maglev","hash_header":"bad header",{ep}}}"#
            )),
            "hash_header",
        ),
        (
            base(&format!(r#"{{"name":"a","hash_header":"X-Id",{ep}}}"#)),
            "only used with the maglev",
        ),
        (
            base(r#"{"name":"a","endpoints":[]}"#),
            "at least one endpoint",
        ),
        (
            base(r#"{"name":"a","endpoints":[{"address":"h"}]}"#),
            "not host:port",
        ),
        (
            base(r#"{"name":"a","endpoints":[{"address":"h:0"}]}"#),
            "port",
        ),
        (
            base(r#"{"name":"a","endpoints":[{"address":"h:01"}]}"#),
            "port",
        ),
        (
            base(r#"{"name":"a","endpoints":[{"address":"-h:1"}]}"#),
            "not a valid DNS name",
        ),
        (
            base(r#"{"name":"a","endpoints":[{"address":"h_x:1"}]}"#),
            "not a valid DNS name",
        ),
        (
            base(r#"{"name":"a","endpoints":[{"address":":1"}]}"#),
            "1 to 253",
        ),
        (
            base(r#"{"name":"a","endpoints":[{"address":"h:1"},{"address":"h:1"}]}"#),
            "duplicate address",
        ),
        (
            base(r#"{"name":"a","endpoints":[{"address":"h:1","weight":0}]}"#),
            "weight: must be between 1 and 1000",
        ),
        (
            base(r#"{"name":"a","endpoints":[{"address":"h:1","priority":8}]}"#),
            "priority",
        ),
        (with(r#""max_requests":0"#), "max_requests"),
        (with(r#""connect_timeout":"0s""#), "connect_timeout"),
        (
            with(r#""panic_threshold_percent":101"#),
            "panic_threshold_percent",
        ),
        (with(r#""health_check":{"path":"healthz"}"#), "path"),
        (with(r#""health_check":{"path":"/a b"}"#), "path"),
        (with(r#""health_check":{"path":"/","host":"a b"}"#), "host"),
        (
            with(r#""health_check":{"path":"/","interval":"1s","timeout":"2s"}"#),
            "must not exceed interval",
        ),
        (
            with(r#""health_check":{"path":"/","interval":"1s","jitter":"2s"}"#),
            "must not exceed interval",
        ),
        (
            with(r#""health_check":{"path":"/","healthy_threshold":0}"#),
            "healthy_threshold",
        ),
        (
            with(r#""outlier_detection":{"base_ejection_time":"10s","max_ejection_time":"5s"}"#),
            "not be less than base",
        ),
        (
            with(r#""outlier_detection":{"success_rate_stdev_factor":0}"#),
            "stdev_factor",
        ),
        (
            with(r#""outlier_detection":{"max_ejection_percent":-1}"#),
            "max_ejection_percent",
        ),
        (with(r#""retry":{"max_retries":6}"#), "max_retries"),
        (with(r#""retry":{"retry_on":[404]}"#), "5xx"),
        (
            with(r#""retry":{"retry_on":[503,503]}"#),
            "duplicate status",
        ),
        (
            with(r#""timeout":"1s","retry":{"per_try_timeout":"2s"}"#),
            "must not exceed the cluster timeout",
        ),
        (
            with(r#""retry":{"backoff_base":"1s","backoff_max":"10ms"}"#),
            "backoff_max",
        ),
        (with(r#""retry":{"max_body_bytes":-1}"#), "max_body_bytes"),
        (with(r#""retry":{"budget":{"ratio":1.5}}"#), "ratio"),
        (
            with(r#""retry":{"budget":{"window":"1500ms"}}"#),
            "whole number of seconds",
        ),
        (
            with(r#""retry":{"budget":{"min_per_second":-1}}"#),
            "min_per_second",
        ),
        (
            format!(
                r#"{{"listen":":80","routes":[{{"cluster":"b"}}],"clusters":[{{"name":"a",{ep}}}]}}"#
            ),
            "unknown cluster",
        ),
        (
            route(r#"{"cluster":"a"},{"cluster":"a"}"#),
            "duplicate route",
        ),
        (route(r#"{"cluster":"a","host":"a.com:80"}"#), "no port"),
        (route(r#"{"cluster":"a","host":"*.*.com"}"#), "*.suffix"),
        (
            route(r#"{"cluster":"a","path_prefix":"api"}"#),
            "must start with",
        ),
        (
            route(r#"{"cluster":"a","path_prefix":"/a/../b"}"#),
            "segments",
        ),
        (route(r#"{"cluster":"a","path_prefix":"//a"}"#), "segments"),
        (
            route(r#"{"cluster":"a","path_prefix":"/a%2f"}"#),
            "must not contain",
        ),
        (
            route(&format!(
                r#"{{"cluster":"a","path_prefix":"/{}"}}"#,
                "a".repeat(1100)
            )),
            "at most 1024",
        ),
    ];
    for (input, want) in cases {
        match parse(&input) {
            Ok(_) => panic!("accepted {input}"),
            Err(e) => assert!(
                e.to_string().contains(want),
                "{input}\n  err = {e}\n  want it to mention {want:?}"
            ),
        }
    }
}

#[test]
fn errors_list_every_problem_with_its_path() {
    let err = parse(
        r#"{"listen":"x","routes":[{"cluster":"b","host":"a b"}],
            "clusters":[{"name":"a","endpoints":[{"address":"h:1","weight":0}]}]}"#,
    )
    .unwrap_err();
    let paths: Vec<_> = err.field_errors().iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "listen",
            "clusters[0].endpoints[0].weight",
            "routes[0].host",
            "routes[0].cluster"
        ]
    );
    assert!(
        err.to_string()
            .starts_with("invalid configuration:\nlisten: ")
    );

    let err = parse(r#"{"clusters":[{"retry":{"retry_on":["503"]}}]}"#).unwrap_err();
    assert!(matches!(
        err,
        ConfigError::Decode {
            format: Format::Json,
            ..
        }
    ));
    assert_eq!(err.field_errors()[0].path, "clusters[0].retry.retry_on[0]");
    assert_eq!(
        err.to_string(),
        "invalid JSON: clusters[0].retry.retry_on[0]: expected an integer, found a string"
    );
    let err = parse("{\n\"listen\" ::80}").unwrap_err();
    assert!(matches!(err, ConfigError::Syntax { .. }));
    assert!(err.to_string().contains("line 2, column 11"), "{err}");
    assert!(err.field_errors().is_empty());
}

#[test]
fn caps_the_number_of_errors() {
    let endpoints = vec![r#"{"address":"bad"}"#; 40].join(",");
    let input = format!(
        r#"{{"listen":":80","routes":[{{"cluster":"a"}}],"clusters":[{{"name":"a","endpoints":[{endpoints}]}}]}}"#
    );
    let err = parse(&input).unwrap_err().to_string();
    assert!(err.contains("more errors)"), "{err}");
    assert_eq!(err.matches('\n').count(), limits::MAX_ERRORS + 1);
}

#[test]
fn enforces_size_and_count_limits() {
    let big = vec![b' '; limits::MAX_FILE_BYTES + 1];
    let err = Config::from_json(&big).unwrap_err();
    assert!(err.to_string().contains("larger"), "{err}");
    assert!(
        Config::from_toml(&big)
            .unwrap_err()
            .to_string()
            .contains("larger")
    );

    let dir = tempdir();
    let path = dir.join("big.json");
    std::fs::write(&path, vec![b' '; limits::MAX_FILE_BYTES + 10]).unwrap();
    let err = Config::load(&path).unwrap_err();
    assert!(err.to_string().contains("big.json is larger"), "{err}");
    let err = Config::load(dir.join("missing.json")).unwrap_err();
    assert!(matches!(err, ConfigError::Io { .. }));
    assert!(std::error::Error::source(&err).is_some());
    let ok = dir.join("ok.json");
    std::fs::write(&ok, FULL).unwrap();
    Config::load(&ok).unwrap();
    assert!(matches!(
        Config::load(&dir).unwrap_err(),
        ConfigError::Io { .. }
    ));

    let clusters: Vec<String> = (0..=limits::MAX_CLUSTERS)
        .map(|i| {
            format!(
                r#"{{"name":"c{}","endpoints":[{{"address":"h:1"}}]}}"#,
                "x".repeat(i % 3)
            )
        })
        .collect();
    let input = format!(
        r#"{{"listen":":80","routes":[{{"cluster":"c"}}],"clusters":[{}]}}"#,
        clusters.join(",")
    );
    let err = parse(&input).unwrap_err().to_string();
    assert!(err.contains("at most 256 clusters"), "{err}");

    let routes: Vec<String> = (0..=limits::MAX_ROUTES)
        .map(|i| format!(r#"{{"cluster":"a","path_prefix":"/{i}"}}"#))
        .collect();
    let input = format!(
        r#"{{"listen":":80","routes":[{}],"clusters":[{{"name":"a","endpoints":[{{"address":"h:1"}}]}}]}}"#,
        routes.join(",")
    );
    assert!(
        parse(&input)
            .unwrap_err()
            .to_string()
            .contains("at most 1024 routes")
    );

    let endpoints: Vec<String> = (0..=limits::MAX_ENDPOINTS)
        .map(|i| format!(r#"{{"address":"h:{}"}}"#, i + 1))
        .collect();
    let input = format!(
        r#"{{"listen":":80","routes":[{{"cluster":"a"}}],"clusters":[{{"name":"a","endpoints":[{}]}}]}}"#,
        endpoints.join(",")
    );
    assert!(
        parse(&input)
            .unwrap_err()
            .to_string()
            .contains("at most 1024 endpoints")
    );

    let proxies = vec![r#""10.0.0.1""#; limits::MAX_TRUSTED_PROXIES + 1].join(",");
    let input = MINIMAL.replacen('{', &format!(r#"{{"trusted_proxies":[{proxies}],"#), 1);
    assert!(
        parse(&input)
            .unwrap_err()
            .to_string()
            .contains("at most 64 entries")
    );
}

#[test]
fn parses_tls() {
    let pre = r#"{"listen":":443","routes":[{"cluster":"a"}],"clusters":[{"name":"a","endpoints":[{"address":"h:1"}]}],"tls":"#;
    let tls = |t: &str| parse(&format!("{pre}{t}}}"));

    let c = tls(r#"{"certificates":[{"cert_file":"a.crt","key_file":"a.key"}]}"#).unwrap();
    let t = c.tls.unwrap();
    assert_eq!(t.min_version, TlsVersion::Tls12);
    assert_eq!(t.handshake_timeout, defaults::HANDSHAKE_TIMEOUT);
    assert_eq!(
        t.max_concurrent_handshakes,
        defaults::MAX_CONCURRENT_HANDSHAKES
    );
    assert!(!t.strict_sni);
    let suites = t.cipher_suites.unwrap();
    assert_eq!(suites.len(), 6);
    assert!(suites.iter().all(|s| !s.name.contains("CBC")));
    assert_eq!(suites[0].id, 0xc02b);
    assert_eq!(t.certificates[0].server_names, None);

    let c = tls(
        r#"{"certificates":[{"cert_file":"/a.crt","key_file":"/a.key","server_names":["API.example.com","*.example.org"]}],
            "min_version":"1.2","cipher_policy":"compatible","handshake_timeout":"2s","max_concurrent_handshakes":10,"strict_sni":true}"#,
    )
    .unwrap();
    let t = c.tls.unwrap();
    assert_eq!(t.cipher_suites.as_ref().unwrap().len(), 10);
    assert_eq!(t.handshake_timeout, ms(2000));
    assert_eq!(t.max_concurrent_handshakes, 10);
    assert!(t.strict_sni);
    assert_eq!(
        t.certificates[0].server_names.as_deref().unwrap(),
        ["api.example.com", "*.example.org"]
    );

    let c = tls(
        r#"{"certificates":[{"cert_file":"a","key_file":"b"}],"cipher_suites":["TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256","TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"]}"#,
    )
    .unwrap();
    let ids: Vec<u16> = c
        .tls
        .unwrap()
        .cipher_suites
        .unwrap()
        .iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(ids, [0xcca8, 0xc02f]);

    let c =
        tls(r#"{"certificates":[{"cert_file":"a","key_file":"b"}],"min_version":"1.3"}"#).unwrap();
    let t = c.tls.unwrap();
    assert_eq!(t.min_version, TlsVersion::Tls13);
    assert_eq!(t.cipher_suites, None);

    let cert = r#""certificates":[{"cert_file":"a","key_file":"b"}]"#;
    let cases: Vec<(String, &str)> = vec![
        ("{}".into(), "tls.certificates: at least one certificate"),
        (r#"{"certificates":[{}]}"#.into(), "cert_file: is required"),
        (
            r#"{"certificates":[{"cert_file":"a"}]}"#.into(),
            "key_file: is required",
        ),
        (format!(r#"{{{cert},"min_version":"1.1"}}"#), "min_version"),
        (
            format!(r#"{{{cert},"cipher_policy":"weak"}}"#),
            "cipher_policy",
        ),
        (
            format!(r#"{{{cert},"cipher_suites":[]}}"#),
            "cipher_suites: must not be empty",
        ),
        (
            format!(r#"{{{cert},"cipher_suites":["x"]}}"#),
            "not a supported secure cipher suite",
        ),
        (
            format!(r#"{{{cert},"cipher_suites":["TLS_RSA_WITH_RC4_128_SHA"]}}"#),
            "not a supported secure",
        ),
        (
            format!(r#"{{{cert},"cipher_suites":["TLS_RSA_WITH_AES_128_GCM_SHA256"]}}"#),
            "not a supported secure",
        ),
        (
            format!(r#"{{{cert},"cipher_suites":["TLS_AES_128_GCM_SHA256"]}}"#),
            "TLS 1.3 suite",
        ),
        (
            format!(r#"{{{cert},"cipher_policy":"modern","cipher_suites":["x"]}}"#),
            "cannot be combined",
        ),
        (
            format!(r#"{{{cert},"min_version":"1.3","cipher_policy":"modern"}}"#),
            "not configurable",
        ),
        (
            format!(r#"{{{cert},"handshake_timeout":"10ms"}}"#),
            "handshake_timeout",
        ),
        (
            format!(r#"{{{cert},"handshake_timeout":"2m"}}"#),
            "handshake_timeout",
        ),
        (
            format!(r#"{{{cert},"max_concurrent_handshakes":0}}"#),
            "max_concurrent_handshakes",
        ),
        (
            r#"{"certificates":[{"cert_file":"a","key_file":"b","server_names":[]}]}"#.into(),
            "must not be empty",
        ),
        (
            r#"{"certificates":[{"cert_file":"a","key_file":"b","server_names":["*"]}]}"#.into(),
            "host name or *.suffix",
        ),
        (
            r#"{"certificates":[{"cert_file":"a","key_file":"b","server_names":["a:1"]}]}"#.into(),
            "host name or *.suffix",
        ),
        (
            r#"{"certificates":[{"cert_file":"a","key_file":"b","server_names":["*.*.a"]}]}"#
                .into(),
            "host name or *.suffix",
        ),
        (
            format!(
                r#"{{{cert},"cipher_suites":["TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256","TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"]}}"#
            ),
            "duplicate cipher suite",
        ),
        (format!(r#"{{{cert},"ocsp":true}}"#), "unknown field"),
    ];
    for (input, want) in cases {
        match tls(&input) {
            Ok(_) => panic!("accepted tls {input}"),
            Err(e) => assert!(
                e.to_string().contains(want),
                "tls {input}\n  err = {e}\n  want it to mention {want:?}"
            ),
        }
    }

    let many = vec![r#"{"cert_file":"a","key_file":"b"}"#; limits::MAX_CERTIFICATES + 1].join(",");
    let err = tls(&format!(r#"{{"certificates":[{many}]}}"#)).unwrap_err();
    assert!(
        err.to_string().contains("at most 256 certificates"),
        "{err}"
    );
    let names = vec![r#""a.com""#; limits::MAX_SERVER_NAMES_PER_CERT + 1].join(",");
    let err = tls(&format!(
        r#"{{"certificates":[{{"cert_file":"a","key_file":"b","server_names":[{names}]}}]}}"#
    ))
    .unwrap_err();
    assert!(err.to_string().contains("at most 100 names"), "{err}");
}

#[test]
fn load_resolves_certificate_paths_like_go() {
    let dir = tempdir();
    let js = r#"{"listen":":443","routes":[{"cluster":"a"}],"clusters":[{"name":"a","endpoints":[{"address":"h:1"}]}],
        "tls":{"certificates":[{"cert_file":"certs/../certs/a.crt","key_file":"/abs/a.key"}]}}"#;
    let path = dir.join("steer.json");
    std::fs::write(&path, js).unwrap();
    let c = Config::load(&path).unwrap();
    let cert = &c.tls.as_ref().unwrap().certificates[0];
    assert_eq!(
        Path::new(&cert.cert_file),
        dir.join("certs").join("a.crt").as_path()
    );
    assert_eq!(cert.key_file, "/abs/a.key");

    // The same in TOML, and from a relative config path.
    let toml = "listen = ':443'\n[[routes]]\ncluster = 'a'\n[[clusters]]\nname = 'a'\nendpoints = [{address = 'h:1'}]\n\
                [tls]\ncertificates = [{cert_file = 'a.crt', key_file = 'b.key'}]\n";
    let path = dir.join("steer.toml");
    std::fs::write(&path, toml).unwrap();
    let c = Config::load(&path).unwrap();
    let cert = &c.tls.as_ref().unwrap().certificates[0];
    assert_eq!(Path::new(&cert.cert_file), dir.join("a.crt").as_path());
    let cwd = std::env::current_dir().unwrap();
    let rel = path.strip_prefix(&cwd).map(Path::to_path_buf).ok();
    if let Some(rel) = rel {
        let c = Config::load(&rel).unwrap();
        let cert = &c.tls.as_ref().unwrap().certificates[0];
        assert!(Path::new(&cert.cert_file).is_relative());
    }
}

/// Every example steer ships loads, and its TOML twin loads to the same
/// config.
#[test]
fn every_steer_example_loads_in_json_and_toml() {
    let mut seen = 0;
    for entry in std::fs::read_dir(steer_examples()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let json = Config::load(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let twin = fixtures().join(path.with_extension("toml").file_name().unwrap());
        let toml = Config::load(&twin).unwrap_or_else(|e| panic!("{}: {e}", twin.display()));
        assert_eq!(json, toml, "{}", path.display());
        seen += 1;
    }
    assert!(seen >= 3, "only {seen} examples");
}

#[test]
fn toml_errors_carry_field_paths() {
    let err = Config::from_toml(b"listen = ':80'\n[[clusters]]\nname = 'a'\nmax_requests = 1.0\n")
        .unwrap_err();
    assert!(matches!(
        err,
        ConfigError::Decode {
            format: Format::Toml,
            ..
        }
    ));
    assert_eq!(
        err.to_string(),
        "invalid TOML: clusters[0].max_requests: expected an integer, found 1.0"
    );
    let err =
        Config::from_toml(b"listen = ':80'\n[[clusters]]\nname = 'a'\ntimeout = 5\n").unwrap_err();
    assert_eq!(err.field_errors()[0].path, "clusters[0].timeout");
    let err = Config::from_toml(b"listen = ':80'\nextra = 1\n").unwrap_err();
    assert!(err.to_string().contains("unknown field \"extra\""), "{err}");
    let err = Config::from_toml(b"listen = 1979-05-27\n").unwrap_err();
    assert_eq!(err.field_errors()[0].path, "listen");
    let err = Config::from_toml(b"listen = \n").unwrap_err();
    assert!(err.to_string().starts_with("invalid TOML: "), "{err}");
    let err = Config::from_toml(b"listen = ':80'\n[[routes]]\ncluster = 'x'\n").unwrap_err();
    assert!(
        err.to_string()
            .contains("routes[0].cluster: unknown cluster"),
        "{err}"
    );
    assert_eq!(Format::from_path(Path::new("a/b.TOML")), Format::Toml);
    assert_eq!(Format::from_path(Path::new("a/b.conf")), Format::Json);
    assert_eq!(Format::from_path(Path::new("toml")), Format::Json);
}

/// Edge cases run through steer's own parser; ours must agree on each.
#[test]
fn agrees_with_steer_on_the_edge_case_corpus() {
    let corpus = std::fs::read_to_string(fixtures().join("steer_verdicts.txt")).unwrap();
    let mut checked = 0;
    let mut disagreements = Vec::new();
    for line in corpus.lines().filter(|l| !l.starts_with('#')) {
        let (verdict, hex) = line.split_once(' ').unwrap();
        let doc: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let ours = Config::from_json(&doc);
        if ours.is_ok() != (verdict == "accept") {
            disagreements.push(format!(
                "steer: {verdict}, ours: {:?}\n  {}",
                ours.err().map(|e| e.to_string()),
                String::from_utf8_lossy(&doc)
            ));
        }
        checked += 1;
    }
    assert!(checked > 200, "{checked}");
    assert!(disagreements.is_empty(), "{}", disagreements.join("\n"));
}

#[test]
fn ip_prefixes_match_addresses() {
    let c = parse(&MINIMAL.replacen(
        '{',
        r#"{"trusted_proxies":["10.0.0.0/8","fd00::/8","192.0.2.1","0.0.0.0/0"],"#,
        1,
    ))
    .unwrap();
    let p = &c.trusted_proxies;
    assert!(p[0].contains("10.1.2.3".parse().unwrap()));
    assert!(!p[0].contains("11.0.0.1".parse().unwrap()));
    assert!(!p[0].contains("::ffff:10.0.0.1".parse().unwrap()));
    assert!(p[1].contains("fd12::1".parse().unwrap()));
    assert!(!p[1].contains("10.0.0.1".parse().unwrap()));
    assert!(p[2].contains("192.0.2.1".parse().unwrap()));
    assert!(!p[2].contains("192.0.2.2".parse().unwrap()));
    assert_eq!((p[2].len(), p[2].is_empty()), (32, false));
    assert!(p[3].is_empty());
    assert!(p[3].contains("203.0.113.9".parse().unwrap()));
    assert_eq!(p[1].addr(), "fd00::".parse::<std::net::IpAddr>().unwrap());
}

#[test]
fn serializes_back_to_steer_json() {
    let c = parse(FULL).unwrap();
    let json = serde_json::to_value(&c).unwrap();
    assert_eq!(json["trusted_proxies"][0], "10.0.0.0/8");
    assert_eq!(json["clusters"][0]["policy"], "maglev");
    assert_eq!(json["clusters"][0]["connect_timeout"], "250ms");
    assert_eq!(
        json["clusters"][0]["outlier_detection"]["max_ejection_time"],
        "5m0s"
    );
    assert_eq!(json["clusters"][1]["timeout"], "15s");
    assert!(json.get("tls").is_none());
    assert_eq!(parse(&json.to_string()).unwrap(), c);

    let tls = MINIMAL.replacen(
        '{',
        r#"{"tls":{"certificates":[{"cert_file":"a","key_file":"b","server_names":["x.com"]}],"cipher_policy":"compatible"},"#,
        1,
    );
    let c = parse(&tls).unwrap();
    let json = serde_json::to_value(&c).unwrap();
    assert_eq!(json["tls"]["min_version"], "1.2");
    assert_eq!(
        json["tls"]["cipher_suites"][6],
        "TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA"
    );
    assert_eq!(parse(&json.to_string()).unwrap(), c);
    let c13 =
        parse(&tls.replace(r#""cipher_policy":"compatible""#, r#""min_version":"1.3""#)).unwrap();
    let json = serde_json::to_value(&c13).unwrap();
    assert!(json["tls"].get("cipher_suites").is_none());
    assert_eq!(parse(&json.to_string()).unwrap(), c13);
}

fn tempdir() -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "edgefront-config-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// Generated configs: anything accepted serializes back to an equal config,
// and accepted configs satisfy what the proxy relies on. Each choice is
// valid most of the time, so that about half the documents are accepted
// and the rest exercise one or two rejections each.

/// Mostly `valid`, occasionally `invalid`.
fn mostly<T: std::fmt::Debug + Clone + 'static>(
    valid: impl Strategy<Value = T> + 'static,
    invalid: impl Strategy<Value = T> + 'static,
) -> BoxedStrategy<T> {
    prop_oneof![40 => valid, 1 => invalid].boxed()
}

/// A duration between `lo_ms` and `hi_ms`, in one of Go's spellings.
fn duration(lo_ms: u64, hi_ms: u64) -> BoxedStrategy<String> {
    mostly(
        (lo_ms..=hi_ms).prop_flat_map(|n| {
            prop_oneof![
                Just(format!("{n}ms")),
                Just(format!("{}us", n * 1000)),
                Just(if n % 1000 == 0 {
                    format!("{}s", n / 1000)
                } else {
                    format!("{}.{:03}s", n / 1000, n % 1000)
                }),
            ]
        }),
        prop_oneof![
            Just("-1s".to_owned()),
            Just("0".to_owned()),
            Just("2h".to_owned())
        ],
    )
}

fn opt<T: std::fmt::Debug + Clone + 'static>(
    s: impl Strategy<Value = T> + 'static,
) -> BoxedStrategy<Option<T>> {
    prop_oneof![Just(None), s.prop_map(Some)].boxed()
}

fn int(lo: i64, hi: i64) -> BoxedStrategy<i64> {
    mostly(lo..=hi, prop_oneof![Just(lo - 1), Just(hi + 1)])
}

fn field<T: serde::Serialize>(name: &str, v: &Option<T>) -> Option<String> {
    v.as_ref()
        .map(|v| format!("\"{name}\":{}", serde_json::to_string(v).unwrap()))
}

fn object(fields: Vec<Option<String>>) -> String {
    format!(
        "{{{}}}",
        fields.into_iter().flatten().collect::<Vec<_>>().join(",")
    )
}

fn endpoint(i: usize) -> BoxedStrategy<String> {
    (
        mostly(
            prop_oneof![
                Just(format!("h{i}.internal:80")),
                Just(format!("10.0.0.{}:8080", i + 1)),
                Just(format!("[fd00::{}]:443", i + 1)),
            ],
            prop_oneof![
                Just("h:0".to_owned()),
                Just("h".to_owned()),
                Just("h_1:1".to_owned())
            ],
        ),
        opt(int(1, 1000)),
        opt(int(0, 7)),
    )
        .prop_map(|(a, w, p)| {
            object(vec![
                Some(format!("\"address\":{a:?}")),
                field("weight", &w),
                field("priority", &p),
            ])
        })
        .boxed()
}

fn health() -> BoxedStrategy<String> {
    // Interval 1s to 5s, jitter and timeout at most 1s, so they fit.
    (
        mostly(Just("/healthz".to_owned()), Just("healthz".to_owned())),
        opt(duration(1000, 5000)),
        opt(duration(0, 1000)),
        opt(duration(1, 1000)),
        opt(int(1, 100)),
        opt(mostly(Just("health.internal"), Just("a b"))),
    )
        .prop_map(|(path, i, j, t, h, host)| {
            object(vec![
                Some(format!("\"path\":{path:?}")),
                field("interval", &i),
                field("jitter", &j),
                field("timeout", &t),
                field("healthy_threshold", &h),
                field("host", &host),
            ])
        })
        .boxed()
}

fn outlier() -> BoxedStrategy<String> {
    (
        opt(int(0, 1000)),
        opt(duration(10, 60_000)),
        opt(duration(1, 30_000)),
        opt(duration(30_000, 600_000)),
        opt(int(0, 100)),
        opt(mostly(0.1f64..10.0, Just(0.0))),
    )
        .prop_map(|(failures, interval, base, max, pct, factor)| {
            object(vec![
                field("consecutive_gateway_failures", &failures),
                field("interval", &interval),
                field("base_ejection_time", &base),
                field("max_ejection_time", &max),
                field("max_ejection_percent", &pct),
                field("success_rate_stdev_factor", &factor),
            ])
        })
        .boxed()
}

fn retry() -> BoxedStrategy<String> {
    (
        opt(int(0, 5)),
        opt(mostly(
            prop::sample::subsequence(vec![500i64, 502, 503, 504], 0..=4),
            Just(vec![404]),
        )),
        opt(duration(0, 500)),
        opt(duration(1, 100)),
        opt(duration(250, 1000)),
        opt(int(0, 1 << 20)),
        opt((
            opt(mostly(0.0f64..=1.0, Just(1.5))),
            opt(int(0, 100_000)),
            opt(mostly(
                (1u64..=60).prop_map(|s| format!("{s}s")),
                Just("1500ms".to_owned()),
            )),
        )),
    )
        .prop_map(|(n, on, per_try, base, max, body, budget)| {
            let budget = budget.map(|(ratio, min, window)| {
                object(vec![
                    field("ratio", &ratio),
                    field("min_per_second", &min),
                    field("window", &window),
                ])
            });
            object(vec![
                field("max_retries", &n),
                field("retry_on", &on),
                field("per_try_timeout", &per_try),
                field("backoff_base", &base),
                field("backoff_max", &max),
                field("max_body_bytes", &body),
                budget.map(|b| format!("\"budget\":{b}")),
            ])
        })
        .boxed()
}

fn cluster(name: &'static str) -> BoxedStrategy<String> {
    (
        (1usize..4).prop_flat_map(|n| (0..n).map(endpoint).collect::<Vec<_>>()),
        prop_oneof![
            Just((None, None)),
            Just((Some("round_robin"), None)),
            Just((Some("least_loaded"), None)),
            Just((Some("maglev"), Some("x-user-id"))),
            Just((Some("maglev"), Some("X-TENANT"))),
            Just((Some("maglev"), None)),
            Just((Some("random"), None)),
            Just((None, Some("x-user-id"))),
        ],
        opt(duration(1, 60_000)),
        opt(duration(1000, 3_600_000)),
        opt(int(1, 1_000_000)),
        opt(int(0, 100)),
        opt(health()),
        opt(outlier()),
        opt(retry()),
    )
        .prop_map(move |(eps, (policy, hash), ct, t, max, panic, h, o, r)| {
            object(vec![
                Some(format!("\"name\":{name:?}")),
                Some(format!("\"endpoints\":[{}]", eps.join(","))),
                field("policy", &policy),
                field("hash_header", &hash),
                field("connect_timeout", &ct),
                field("timeout", &t),
                field("max_requests", &max),
                field("panic_threshold_percent", &panic),
                h.map(|h| format!("\"health_check\":{h}")),
                o.map(|o| format!("\"outlier_detection\":{o}")),
                r.map(|r| format!("\"retry\":{r}")),
            ])
        })
        .boxed()
}

fn route(i: usize) -> BoxedStrategy<String> {
    (
        opt(mostly(
            prop_oneof![
                Just(format!("Host{i}.example.com")),
                Just(format!("*.h{i}.example.com")),
            ],
            Just("a.com:80".to_owned()),
        )),
        mostly(Just(format!("/r{i}/")), Just("/a/../b".to_owned())),
        mostly(Just("a"), Just("missing")),
    )
        .prop_map(|(host, prefix, cluster)| {
            object(vec![
                field("host", &host),
                Some(format!("\"path_prefix\":{prefix:?}")),
                Some(format!("\"cluster\":{cluster:?}")),
            ])
        })
        .boxed()
}

fn tls() -> BoxedStrategy<String> {
    (
        prop_oneof![Just(None), Just(Some("1.2")), Just(Some("1.3"))],
        prop_oneof![
            4 => Just((None, None)),
            2 => Just((Some("compatible"), None)),
            2 => Just((None, Some(vec!["TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256", "TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA"]))),
            1 => Just((None, Some(vec!["TLS_AES_128_GCM_SHA256"]))),
        ],
        opt(mostly(
            Just(vec!["A.example.com", "*.b.org"]),
            Just(vec!["*"]),
        )),
        opt(duration(100, 60_000)),
        opt(int(1, 100_000)),
        any::<bool>(),
    )
        .prop_map(|(min, (policy, suites), names, handshake, max, strict)| {
            let cert = object(vec![
                Some("\"cert_file\":\"a.crt\",\"key_file\":\"a.key\"".to_owned()),
                field("server_names", &names),
            ]);
            object(vec![
                Some(format!("\"certificates\":[{cert}]")),
                field("min_version", &min),
                field("cipher_policy", &policy),
                field("cipher_suites", &suites),
                field("handshake_timeout", &handshake),
                field("max_concurrent_handshakes", &max),
                Some(format!("\"strict_sni\":{strict}")),
            ])
        })
        .boxed()
}

fn document() -> BoxedStrategy<String> {
    (
        mostly(
            prop_oneof![Just(":8080"), Just("127.0.0.1:0"), Just("[::1]:443")],
            Just("example.com:80"),
        ),
        opt(prop::sample::subsequence(
            vec!["10.0.0.0/8", "192.168.1.7", "fd00::/8", "::1"],
            0..=4,
        )),
        (1usize..4).prop_flat_map(|n| (0..n).map(route).collect::<Vec<_>>()),
        cluster("a"),
        opt(cluster("b")),
        opt(tls()),
    )
        .prop_map(|(listen, proxies, routes, a, b, tls)| {
            let clusters = [Some(a), b].into_iter().flatten().collect::<Vec<_>>();
            object(vec![
                Some(format!("\"listen\":{listen:?}")),
                field("trusted_proxies", &proxies),
                Some(format!("\"routes\":[{}]", routes.join(","))),
                Some(format!("\"clusters\":[{}]", clusters.join(","))),
                tls.map(|t| format!("\"tls\":{t}")),
            ])
        })
        .boxed()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn accepted_configs_round_trip_and_hold_invariants(doc in document()) {
        let Ok(c) = parse(&doc) else { return Ok(()) };
        let json = serde_json::to_string(&c).unwrap();
        prop_assert_eq!(&parse(&json).unwrap(), &c, "{}", json);

        let names: Vec<&str> = c.clusters.iter().map(|c| c.name.as_str()).collect();
        for cl in &c.clusters {
            prop_assert!(!cl.endpoints.is_empty() && cl.max_requests >= 1);
            for e in &cl.endpoints {
                prop_assert!((1..=limits::MAX_WEIGHT).contains(&e.weight));
                prop_assert!(e.priority <= limits::MAX_PRIORITY);
            }
            prop_assert_eq!(cl.policy == Policy::Maglev, cl.hash_header.is_some());
            if let Some(r) = &cl.retry {
                prop_assert!(!r.backoff_base.is_zero() && r.backoff_max >= r.backoff_base);
                prop_assert!(r.budget.window >= Duration::from_secs(1));
            }
            if let Some(h) = &cl.health_check {
                prop_assert!(!h.interval.is_zero() && h.timeout <= h.interval && h.healthy_threshold >= 1);
            }
            if let Some(o) = &cl.outlier_detection {
                prop_assert!(!o.base_ejection_time.is_zero() && o.max_ejection_time >= o.base_ejection_time);
            }
        }
        if let Some(t) = &c.tls {
            prop_assert!(!t.certificates.is_empty());
            prop_assert!(t.handshake_timeout >= limits::MIN_HANDSHAKE_TIMEOUT);
            prop_assert_eq!(t.min_version == TlsVersion::Tls13, t.cipher_suites.is_none());
            for cert in &t.certificates {
                if let Some(names) = &cert.server_names {
                    prop_assert!(!names.is_empty());
                    prop_assert!(names.iter().all(|n| n != "*" && *n == n.to_lowercase()));
                }
            }
        }
        for r in &c.routes {
            prop_assert!(names.contains(&r.cluster.as_str()));
            prop_assert!(r.path_prefix.starts_with('/') && !r.host.is_empty());
        }
    }

    /// Any bytes at all: an error or a config, never a panic.
    #[test]
    fn arbitrary_input_never_panics(
        doc in prop_oneof![
            prop::collection::vec(any::<u8>(), 0..256),
            (Just(FULL), 0usize..FULL.len(), any::<u8>()).prop_map(|(s, i, b)| {
                let mut v = s.as_bytes().to_vec();
                v[i] = b;
                v
            }),
        ]
    ) {
        let _ = Config::from_json(&doc);
        let _ = Config::from_toml(&doc);
    }
}
