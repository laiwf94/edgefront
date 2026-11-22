use super::*;

const BASIC: &str = include_str!("../../tests/data/envoy_ratelimit/basic_config.yaml");
const WILDCARD: &str = include_str!("../../tests/data/envoy_ratelimit/wildcard.yaml");

fn basic() -> DescriptorConfig {
    DescriptorConfig::load(&[("basic_config.yaml", BASIC)], false).unwrap()
}

/// `GetLimit` as config_test.go checks it: `None`, or the limit's
/// requests per unit, unit and full key.
fn get(
    c: &DescriptorConfig,
    domain: &str,
    entries: &[(&str, &str)],
) -> Option<(u64, Option<Unit>, String)> {
    c.get_limit(domain, entries)
        .map(|l| (l.requests_per_unit, l.unit, l.full_key.clone()))
}

fn lim(rpu: u64, unit: Unit, key: &str) -> Option<(u64, Option<Unit>, String)> {
    Some((rpu, Some(unit), key.to_owned()))
}

/// TestBasicConfig, case by case.
#[test]
fn envoy_ratelimit_basic_config_vectors() {
    let c = basic();
    assert_eq!(get(&c, "foo_domain", &[]), None);
    assert_eq!(get(&c, "test-domain", &[]), None);
    assert_eq!(get(&c, "test-domain", &[("key1", "something")]), None);
    assert_eq!(get(&c, "test-domain", &[("key1", "value1")]), None);
    assert_eq!(
        get(
            &c,
            "test-domain",
            &[("key2", "value2"), ("subkey", "subvalue")]
        ),
        None
    );
    assert_eq!(
        get(
            &c,
            "test-domain",
            &[("key5", "value5"), ("subkey5", "subvalue")]
        ),
        None
    );
    assert_eq!(
        get(
            &c,
            "test-domain",
            &[("key1", "value1"), ("subkey1", "something")]
        ),
        lim(5, Unit::Second, "test-domain.key1_value1.subkey1")
    );
    assert_eq!(
        get(
            &c,
            "test-domain",
            &[("key1", "value1"), ("subkey1", "subvalue1")]
        ),
        lim(
            10,
            Unit::Second,
            "test-domain.key1_value1.subkey1_subvalue1"
        )
    );
    assert_eq!(
        get(&c, "test-domain", &[("key2", "something")]),
        lim(20, Unit::Minute, "test-domain.key2")
    );
    assert_eq!(
        get(&c, "test-domain", &[("key2", "value2")]),
        lim(30, Unit::Minute, "test-domain.key2_value2")
    );
    assert_eq!(get(&c, "test-domain", &[("key2", "value3")]), None);
    let k3 = c.get_limit("test-domain", &[("key3", "foo")]).unwrap();
    assert_eq!(
        (k3.requests_per_unit, k3.unit, k3.name.as_deref()),
        (1, Some(Unit::Hour), None)
    );
    assert_eq!(k3.full_key, "test-domain.key3");
    let k4 = c.get_limit("test-domain", &[("key4", "foo")]).unwrap();
    assert_eq!((k4.requests_per_unit, k4.unit), (1, Some(Unit::Day)));
    assert_eq!(k4.name.as_deref(), Some("key4_rate_limit"));
    let k6 = c.get_limit("test-domain", &[("key6", "foo")]).unwrap();
    assert!(k6.unlimited);
    assert_eq!(k6.full_key, "test-domain.key6");
    assert_eq!(
        get(&c, "test-domain", &[("key7", "unspecified_value")]),
        lim(70, Unit::Minute, "test-domain.key7_unspecified_value")
    );
    assert_eq!(
        get(&c, "test-domain", &[("key7", "another_value")]),
        lim(70, Unit::Minute, "test-domain.key7_another_value")
    );
    assert_eq!(
        get(&c, "test-domain", &[("key8", "x")]),
        lim(200, Unit::Month, "test-domain.key8")
    );
    assert_eq!(c.domains().collect::<Vec<_>>(), ["test-domain"]);
}

/// TestWildcardConfig, case by case.
#[test]
fn envoy_ratelimit_wildcard_vectors() {
    let c = DescriptorConfig::load(&[("wildcard.yaml", WILDCARD)], false).unwrap();
    let key = |entries: &[(&str, &str)]| c.get_limit("test-domain", entries).map(|l| l.full_key);
    assert_eq!(
        key(&[("noVal", "foo1")]).as_deref(),
        Some("test-domain.noVal")
    );
    assert_eq!(
        key(&[("noVal", "foo2")]).as_deref(),
        Some("test-domain.noVal")
    );
    assert_eq!(
        key(&[("wild", "foo1")]).as_deref(),
        Some("test-domain.wild_foo*")
    );
    assert_eq!(
        key(&[("wild", "foo2")]).as_deref(),
        Some("test-domain.wild_foo*")
    );
    assert_eq!(
        key(&[("nestedWild", "val1"), ("wild", "goo2")]).as_deref(),
        Some("test-domain.nestedWild_val1.wild_goo*")
    );
    assert_eq!(key(&[("wild", "bar")]), None);
    assert_eq!(key(&[("noWild", "foo1")]), None);
    for v in ["barab", "bar123b", "barb"] {
        assert_eq!(
            key(&[("midWild", v)]).as_deref(),
            Some("test-domain.midWild_bar*b"),
            "{v}"
        );
    }
    assert_eq!(key(&[("midWild", "bara")]), None);
    assert_eq!(key(&[("midWild", "xbarb")]), None);
    for v in ["foo123bar456baz", "foobarbaz"] {
        assert_eq!(
            key(&[("multiWild", v)]).as_deref(),
            Some("test-domain.multiWild_foo*bar*baz")
        );
    }
    assert_eq!(key(&[("multiWild", "foo123baz")]), None);
    assert_eq!(key(&[("multiWild", "bar456baz")]), None);
    assert!(wildcard_match("a*", "a") && !wildcard_match("ab*ba", "aba"));
}

/// The error configs of config_test.go, with equivalent files.
#[test]
fn envoy_ratelimit_error_messages() {
    let one = |name: &str, text: &str| DescriptorConfig::load(&[(name, text)], false).unwrap_err();
    for (name, text, want) in [
        (
            "empty_domain.yaml",
            "descriptors: []\n",
            "empty_domain.yaml: config file cannot have empty domain",
        ),
        (
            "empty_key.yaml",
            "domain: d\ndescriptors:\n  - value: v\n",
            "empty_key.yaml: descriptor has empty key",
        ),
        (
            "duplicate_key.yaml",
            "domain: test-domain\ndescriptors:\n  - key: key1\n    value: value1\n  - key: key1\n    value: value1\n",
            "duplicate_key.yaml: duplicate descriptor composite key 'test-domain.key1_value1'",
        ),
        (
            "bad_limit_unit.yaml",
            "domain: d\ndescriptors:\n  - key: k\n    rate_limit:\n      unit: foo\n      requests_per_unit: 1\n",
            "bad_limit_unit.yaml: invalid rate limit unit 'foo'",
        ),
        (
            "replaces_self.yaml",
            "domain: d\ndescriptors:\n  - key: k\n    rate_limit:\n      unit: second\n      requests_per_unit: 1\n      name: a\n      replaces:\n        - name: a\n",
            "replaces_self.yaml: replaces should not contain name of same descriptor",
        ),
        (
            "replaces_empty.yaml",
            "domain: d\ndescriptors:\n  - key: k\n    rate_limit:\n      unit: second\n      requests_per_unit: 1\n      replaces:\n        - name: ''\n",
            "replaces_empty.yaml: should not have an empty replaces entry",
        ),
        (
            "bad_yaml.yaml",
            "domain: 'd\n",
            "bad_yaml.yaml: error loading config file: yaml: line 1: found unexpected end of stream",
        ),
        (
            "misspelled_key.yaml",
            "domain: d\ndescriptors:\n  - key: k\n    ratelimit:\n      unit: second\n",
            "misspelled_key.yaml: config error, unknown key 'ratelimit'",
        ),
        (
            "misspelled_key2.yaml",
            "domain: d\ndescriptors:\n  - key: k\n    rate_limit:\n      unit: second\n      requestsperunit: 5\n",
            "misspelled_key2.yaml: config error, unknown key 'requestsperunit'",
        ),
        (
            "non_string_key.yaml",
            "domain: d\ndescriptors:\n  - key: k\n    0.25: x\n",
            "non_string_key.yaml: config error, key is not of type string: 0.25",
        ),
        (
            "non_map_list.yaml",
            "domain: d\ndescriptors:\n  - a\n",
            "non_map_list.yaml: config error, yaml file contains list of type other than map: a",
        ),
        (
            "unlimited_with_unit.yaml",
            "domain: d\ndescriptors:\n  - key: k\n    rate_limit:\n      unlimited: true\n      unit: day\n",
            "unlimited_with_unit.yaml: should not specify rate limit unit when unlimited",
        ),
    ] {
        assert_eq!(one(name, text), want, "{name}");
    }
    let dup = DescriptorConfig::load(
        &[
            ("basic_config.yaml", BASIC),
            ("duplicate_domain.yaml", "domain: test-domain\n"),
        ],
        false,
    )
    .unwrap_err();
    assert_eq!(
        dup,
        "duplicate_domain.yaml: duplicate domain 'test-domain' in config file"
    );
    // Merged, a second file adds to the domain, but not the same key.
    let m1 = "domain: test-domain\ndescriptors:\n  - key: key1\n    value: value1\n";
    let m2 = "domain: test-domain\ndescriptors:\n  - key: other\n";
    assert!(DescriptorConfig::load(&[("a.yaml", m1), ("b.yaml", m2)], true).is_ok());
    assert_eq!(
        DescriptorConfig::load(
            &[
                ("merge_domain_key1.yaml", m1),
                ("merge_domain_key1.yaml", m1)
            ],
            true
        )
        .unwrap_err(),
        "merge_domain_key1.yaml: duplicate descriptor composite key 'test-domain.key1_value1'"
    );
}

#[test]
fn decisions_are_the_services_fixed_windows() {
    let l = DescriptorLimiter::new(basic(), 1 << 16);
    let d: &[(&str, &str)] = &[("key1", "value1"), ("subkey1", "a")];
    // 5 a second; the window is the Unix second.
    let t = 1_700_000_000;
    for n in 1..=5 {
        let r = l.should_rate_limit("test-domain", &[d], 1, t);
        assert!(!r.over_limit, "{n}");
        assert_eq!(r.statuses[0].limit_remaining, 5 - n);
    }
    let r = l.should_rate_limit("test-domain", &[d], 1, t);
    assert!(r.over_limit && r.statuses[0].over_limit);
    assert_eq!(r.statuses[0].reset_secs, 1);
    // Another value of the key-only entry has its own counter.
    let other: &[(&str, &str)] = &[("key1", "value1"), ("subkey1", "b")];
    assert!(
        !l.should_rate_limit("test-domain", &[other], 1, t)
            .over_limit
    );
    // The next second is a new window.
    assert!(
        !l.should_rate_limit("test-domain", &[d], 1, t + 1)
            .over_limit
    );
    // Hits add up, over limit or not: 3 + 3 passes 5.
    let e: &[(&str, &str)] = &[("key1", "value1"), ("subkey1", "c")];
    assert!(!l.should_rate_limit("test-domain", &[e], 3, t).over_limit);
    assert!(l.should_rate_limit("test-domain", &[e], 3, t).over_limit);
    // A minute window counts from the minute.
    let m: &[(&str, &str)] = &[("key2", "x")];
    let start = t - t % 60;
    let r = l.should_rate_limit("test-domain", &[m], 20, start + 59);
    assert_eq!((r.over_limit, r.statuses[0].reset_secs), (false, 1));
    assert!(
        l.should_rate_limit("test-domain", &[m], 1, start + 59)
            .over_limit
    );
    assert!(
        !l.should_rate_limit("test-domain", &[m], 1, start + 60)
            .over_limit
    );
    // Unlimited and unmatched descriptors never refuse; one over limit
    // makes the response over limit.
    let u: &[(&str, &str)] = &[("key6", "x")];
    let none: &[(&str, &str)] = &[("nope", "x")];
    let r = l.should_rate_limit("test-domain", &[u, none, d], 1000, t);
    assert!(r.over_limit);
    assert_eq!(
        r.statuses.iter().map(|s| s.over_limit).collect::<Vec<_>>(),
        [false, false, true]
    );
    assert!(r.statuses[0].limit.as_ref().unwrap().unlimited);
    assert!(r.statuses[1].limit.is_none());
    assert!(format!("{l:?}").contains("domains: 1"));
    assert_eq!(l.config().domains().count(), 1);
    assert_eq!(Unit::Week.secs(), 604_800);
    assert_eq!(Unit::Year.name(), "YEAR");
}

#[test]
fn shadow_mode_replaces_and_shared_wildcards() {
    let cfg = "domain: d\ndescriptors:\n  - key: path\n    value: /login\n    shadow_mode: true\n    rate_limit: {unit: minute, requests_per_unit: 1, name: login}\n    descriptors:\n      - key: user\n        value: \"bot-*\"\n        share_threshold: true\n        rate_limit: {unit: minute, requests_per_unit: 2, replaces: [{name: login}]}\n  - key: user\n    value: \"bot-*\"\n    rate_limit: {unit: minute, requests_per_unit: 1}\n";
    let l = DescriptorLimiter::new(
        DescriptorConfig::load(&[("c.yaml", cfg)], false).unwrap(),
        1024,
    );
    let t = 6_000;
    let login: &[(&str, &str)] = &[("path", "/login")];
    // Shadow mode: counted, never refused.
    for _ in 0..5 {
        let r = l.should_rate_limit("d", &[login], 1, t);
        assert!(!r.over_limit && !r.statuses[0].over_limit);
    }
    // share_threshold: every bot shares the pattern's counter.
    let b1: &[(&str, &str)] = &[("path", "/login"), ("user", "bot-1")];
    let b2: &[(&str, &str)] = &[("path", "/login"), ("user", "bot-2")];
    assert!(!l.should_rate_limit("d", &[b1], 1, t).over_limit);
    assert!(!l.should_rate_limit("d", &[b2], 1, t).over_limit);
    assert!(l.should_rate_limit("d", &[b1], 1, t).over_limit);
    // Without it, each value counts on its own.
    let u1: &[(&str, &str)] = &[("user", "bot-1")];
    let u2: &[(&str, &str)] = &[("user", "bot-2")];
    assert!(!l.should_rate_limit("d", &[u1], 1, t).over_limit);
    assert!(!l.should_rate_limit("d", &[u2], 1, t).over_limit);
    assert!(l.should_rate_limit("d", &[u2], 1, t).over_limit);
    // The bot limit replaces the login limit in a request with both.
    let r = l.should_rate_limit("d", &[login, b2], 1, t + 60);
    assert!(r.statuses[0].limit.is_none(), "replaced");
    assert!(r.statuses[1].limit.is_some());
}

#[test]
fn counters_stay_within_their_bound() {
    let l = DescriptorLimiter::new(basic(), 64);
    for i in 0..10_000u64 {
        let v = i.to_string();
        let d: &[(&str, &str)] = &[("key2", &v)];
        l.should_rate_limit("test-domain", &[d], 1, 1000 + i / 100);
    }
    let held: usize = l
        .counters
        .shards
        .iter()
        .map(|s| s.lock().unwrap().len())
        .sum();
    assert!(held <= 64, "{held}");
}

#[test]
fn decide_takes_per_descriptor_hits_and_overrides() {
    let l = DescriptorLimiter::new(basic(), 1024);
    let t = 1_700_006_400;
    let key3: &[(&str, &str)] = &[("key3", "a")];
    let q = |hits, negative| Query {
        entries: key3,
        hits,
        negative,
        limit_override: None,
    };
    // key3 is 1 an hour: check-only leaves it, a hit uses it, a negative
    // hit gives it back.
    assert_eq!(
        l.decide("test-domain", &[q(0, false)], t).statuses[0].limit_remaining,
        1
    );
    assert!(!l.decide("test-domain", &[q(1, false)], t).over_limit);
    assert!(l.decide("test-domain", &[q(0, false)], t).statuses[0].limit_remaining == 0);
    assert!(!l.decide("test-domain", &[q(1, true)], t).over_limit);
    assert!(!l.decide("test-domain", &[q(1, false)], t).over_limit);
    // An override names its limit by the entries, a key with no value by
    // the key alone, and counts on the configured limit's counter.
    let o = LimitOverride {
        requests_per_unit: 3,
        unit: Unit::Hour,
        unit_multiplier: 2,
    };
    let r = l.decide(
        "test-domain",
        &[Query {
            limit_override: Some(o),
            ..q(1, false)
        }],
        t,
    );
    let limit = r.statuses[0].limit.as_ref().unwrap();
    assert_eq!(limit.full_key, "test-domain.key3_a");
    assert_eq!((limit.requests_per_unit, limit.unit_multiplier), (3, 2));
    assert_eq!(r.statuses[0].reset_secs, 7200 - t % 7200);
    let entries: &[(&str, &str)] = &[("ip", "1.2.3.4"), ("path", "")];
    let r = l.decide(
        "test-domain",
        &[Query {
            entries,
            hits: 1,
            negative: false,
            limit_override: Some(o),
        }],
        t,
    );
    assert_eq!(
        r.statuses[0].limit.as_ref().unwrap().full_key,
        "test-domain.ip_1.2.3.4.path"
    );
    assert_eq!(r.statuses[0].limit_remaining, 2);
}
