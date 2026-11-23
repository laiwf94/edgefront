use super::*;
use crate::descriptor::DescriptorConfig;
use crate::service::proto::envoy::extensions::common::ratelimit::v3::{
    RateLimitDescriptor, rate_limit_descriptor,
};

const BASIC: &str = include_str!("../../../tests/data/envoy_ratelimit/basic_config.yaml");

/// A Unix time on a day boundary, so every window starts here.
const T: u64 = 1_700_006_400;

fn service(yaml: &str) -> RateLimitService {
    let cfg = DescriptorConfig::load(&[("c.yaml", yaml)], false).unwrap();
    RateLimitService::new(Arc::new(DescriptorLimiter::new(cfg, 1 << 16)))
}

fn desc(entries: &[(&str, &str)]) -> RateLimitDescriptor {
    RateLimitDescriptor {
        entries: entries
            .iter()
            .map(|(k, v)| rate_limit_descriptor::Entry {
                key: (*k).to_owned(),
                value: (*v).to_owned(),
            })
            .collect(),
        ..RateLimitDescriptor::default()
    }
}

fn request(domain: &str, descriptors: Vec<RateLimitDescriptor>) -> RateLimitRequest {
    RateLimitRequest {
        domain: domain.to_owned(),
        descriptors,
        hits_addend: 0,
    }
}

fn ok(code: i32) -> bool {
    code == i32::from(Code::Ok)
}

fn over(code: i32) -> bool {
    code == i32::from(Code::OverLimit)
}

fn secs(st: &DescriptorStatus) -> Option<i64> {
    st.duration_until_reset.as_ref().map(|d| {
        assert_eq!(d.nanos, 0);
        d.seconds
    })
}

#[test]
fn requests_without_a_domain_or_descriptors_are_refused() {
    let s = service(BASIC);
    let e = s
        .decide_at(&request("", vec![desc(&[("key1", "value1")])]), T)
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    assert_eq!(e.message(), "rate limit domain must not be empty");
    let e = s.decide_at(&request("test-domain", vec![]), T).unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    assert_eq!(e.message(), "rate limit descriptor list must not be empty");
}

#[test]
fn statuses_carry_the_limit_what_remains_and_the_reset() {
    let s = service(BASIC);
    let d = || desc(&[("key1", "value1"), ("subkey1", "a")]);
    for n in 1..=5u32 {
        let r = s.decide_at(&request("test-domain", vec![d()]), T).unwrap();
        assert!(ok(r.overall_code), "{n}");
        let st = &r.statuses[0];
        assert!(ok(st.code));
        assert_eq!(
            st.current_limit,
            Some(RateLimit {
                name: String::new(),
                requests_per_unit: 5,
                unit: rate_limit::Unit::Second.into(),
                unit_multiplier: None,
            })
        );
        assert_eq!(st.limit_remaining, 5 - n);
        assert_eq!(secs(st), Some(1));
        assert!(st.quota.is_none());
        assert!(r.response_headers_to_add.is_empty());
    }
    let r = s.decide_at(&request("test-domain", vec![d()]), T).unwrap();
    assert!(over(r.overall_code) && over(r.statuses[0].code));
    assert_eq!(r.statuses[0].limit_remaining, 0);
    // A named monthly limit, 15 s into its 30-day window.
    let r = s
        .decide_at(
            &request("test-domain", vec![desc(&[("key8", "x")])]),
            T + 15,
        )
        .unwrap();
    let st = &r.statuses[0];
    let limit = st.current_limit.as_ref().unwrap();
    assert_eq!(limit.name, "key8_rate_limit");
    assert_eq!(limit.unit, i32::from(rate_limit::Unit::Month));
    let month = 30 * 86_400;
    assert_eq!(
        secs(st),
        Some(i64::try_from(month - (T + 15) % month).unwrap())
    );
    assert_eq!(st.limit_remaining, 199);
    // Every unit has its own enum value in the response.
    for (unit, want) in [
        (Unit::Second, rate_limit::Unit::Second),
        (Unit::Minute, rate_limit::Unit::Minute),
        (Unit::Hour, rate_limit::Unit::Hour),
        (Unit::Day, rate_limit::Unit::Day),
        (Unit::Week, rate_limit::Unit::Week),
        (Unit::Month, rate_limit::Unit::Month),
        (Unit::Year, rate_limit::Unit::Year),
    ] {
        assert_eq!(response_unit(unit), want);
    }
}

#[test]
fn unmatched_and_unlimited_descriptors_are_ok_with_no_limit() {
    let s = service(BASIC);
    let r = s
        .decide_at(
            &request(
                "test-domain",
                vec![
                    desc(&[("nope", "x")]),
                    desc(&[("key6", "x")]),
                    desc(&[("key2", "value3")]),
                ],
            ),
            T,
        )
        .unwrap();
    assert!(ok(r.overall_code));
    let none = DescriptorStatus {
        code: Code::Ok.into(),
        ..DescriptorStatus::default()
    };
    assert_eq!(r.statuses[0], none);
    assert_eq!(
        r.statuses[1],
        DescriptorStatus {
            code: Code::Ok.into(),
            limit_remaining: u32::MAX,
            ..DescriptorStatus::default()
        }
    );
    // Listed with no limit: whitelisted.
    assert_eq!(r.statuses[2], none);
    // An unknown domain limits nothing.
    let r = s
        .decide_at(&request("other", vec![desc(&[("key2", "x")])]), T)
        .unwrap();
    assert_eq!(r.statuses, [none]);
}

#[test]
fn hits_come_from_the_descriptor_then_the_request() {
    let s = service(BASIC);
    // key3: 1 an hour.
    let mut req = request("test-domain", vec![desc(&[("key3", "a")])]);
    req.hits_addend = 2;
    let r = s.decide_at(&req, T).unwrap();
    assert!(over(r.overall_code), "2 hits pass a limit of 1");
    // key2: 20 a minute. The descriptor's own hits win over the request's.
    let mut d = desc(&[("key2", "h")]);
    d.hits_addend = Some(protobuf::UInt64Value { value: 15 });
    let mut req = request("test-domain", vec![d.clone()]);
    req.hits_addend = 100;
    assert_eq!(s.decide_at(&req, T).unwrap().statuses[0].limit_remaining, 5);
    // 0 checks without counting.
    d.hits_addend = Some(protobuf::UInt64Value { value: 0 });
    for _ in 0..3 {
        let r = s
            .decide_at(&request("test-domain", vec![d.clone()]), T)
            .unwrap();
        assert_eq!(r.statuses[0].limit_remaining, 5);
    }
    // Negative hits give budget back, never below an empty counter.
    d.hits_addend = Some(protobuf::UInt64Value { value: 10 });
    d.is_negative_hits = true;
    let r = s
        .decide_at(&request("test-domain", vec![d.clone()]), T)
        .unwrap();
    assert_eq!(r.statuses[0].limit_remaining, 15);
    d.hits_addend = Some(protobuf::UInt64Value { value: 1000 });
    let r = s
        .decide_at(&request("test-domain", vec![d.clone()]), T)
        .unwrap();
    assert_eq!(r.statuses[0].limit_remaining, 20);
    // With neither set, a request adds 1.
    let r = s
        .decide_at(&request("test-domain", vec![desc(&[("key2", "h")])]), T)
        .unwrap();
    assert_eq!(r.statuses[0].limit_remaining, 19);
}

#[test]
fn a_limit_override_replaces_the_configured_limit() {
    let s = service(
        "domain: d\ndescriptors:\n  - key: user\n    shadow_mode: true\n    rate_limit: {unit: second, requests_per_unit: 100, name: users}\n",
    );
    let with = |entries: &[(&str, &str)], rpu, unit: RateLimitUnit, mult: Option<u32>| {
        let mut d = desc(entries);
        d.limit = Some(rate_limit_descriptor::RateLimitOverride {
            requests_per_unit: rpu,
            unit: unit.into(),
            unit_multiplier: mult.map(|value| protobuf::UInt32Value { value }),
        });
        d
    };
    // On a descriptor the tree has no limit for: 2 per 10 s.
    let d = with(&[("ip", "1.2.3.4")], 2, RateLimitUnit::Second, Some(10));
    for _ in 0..2 {
        let r = s.decide_at(&request("d", vec![d.clone()]), T + 3).unwrap();
        assert!(ok(r.overall_code));
        let st = &r.statuses[0];
        assert_eq!(
            st.current_limit,
            Some(RateLimit {
                name: String::new(),
                requests_per_unit: 2,
                unit: rate_limit::Unit::Second.into(),
                unit_multiplier: Some(protobuf::UInt32Value { value: 10 }),
            })
        );
        assert_eq!(secs(st), Some(7));
    }
    assert!(over(
        s.decide_at(&request("d", vec![d.clone()]), T + 9)
            .unwrap()
            .overall_code
    ));
    assert!(ok(s
        .decide_at(&request("d", vec![d]), T + 10)
        .unwrap()
        .overall_code));
    // On one in shadow mode: the override is enforced.
    let d = with(&[("user", "u")], 1, RateLimitUnit::Minute, None);
    let r = s.decide_at(&request("d", vec![d.clone(), d]), T).unwrap();
    assert!(over(r.overall_code));
    assert!(ok(r.statuses[0].code) && over(r.statuses[1].code));
    assert_eq!(
        r.statuses[0]
            .current_limit
            .as_ref()
            .unwrap()
            .unit_multiplier,
        None,
        "a multiplier of 1 is not reported"
    );
    // A multiplier of 0 means 1.
    let d = with(&[("ip", "z")], 5, RateLimitUnit::Hour, Some(0));
    let r = s.decide_at(&request("d", vec![d]), T + 60).unwrap();
    assert_eq!(secs(&r.statuses[0]), Some(3600 - 60));
    for (n, unit) in [
        (1, Unit::Second),
        (2, Unit::Minute),
        (3, Unit::Hour),
        (4, Unit::Day),
        (5, Unit::Month),
        (6, Unit::Year),
    ] {
        assert_eq!(override_unit(n), Some(unit));
    }
    // An override with no unit cannot be counted.
    for bad in [0, 7, -1] {
        let mut d = with(&[("ip", "x")], 1, RateLimitUnit::Second, None);
        d.limit.as_mut().unwrap().unit = bad;
        let e = s
            .decide_at(&request("d", vec![desc(&[("ip", "y")]), d]), T)
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        assert_eq!(
            e.message(),
            format!("descriptor 1: limit override has no valid unit ({bad})")
        );
    }
}

#[test]
fn shadow_mode_stays_ok_and_replaced_limits_are_dropped() {
    let s = service(
        "domain: d\ndescriptors:\n  - key: path\n    value: /login\n    shadow_mode: true\n    rate_limit: {unit: minute, requests_per_unit: 1, name: login}\n  - key: user\n    rate_limit: {unit: minute, requests_per_unit: 3, replaces: [{name: login}]}\n",
    );
    let login = || desc(&[("path", "/login")]);
    for _ in 0..3 {
        let r = s.decide_at(&request("d", vec![login()]), T).unwrap();
        assert!(ok(r.overall_code) && ok(r.statuses[0].code));
        assert_eq!(r.statuses[0].limit_remaining, 0);
        assert!(r.statuses[0].current_limit.is_some());
    }
    let r = s
        .decide_at(&request("d", vec![login(), desc(&[("user", "u")])]), T)
        .unwrap();
    assert!(r.statuses[0].current_limit.is_none(), "replaced");
    assert_eq!(r.statuses[1].limit_remaining, 2);
}

#[test]
fn response_headers_describe_the_descriptor_closest_to_its_limit() {
    let cfg = "domain: d\ndescriptors:\n  - key: a\n    rate_limit: {unit: minute, requests_per_unit: 10}\n  - key: b\n    rate_limit: {unit: hour, requests_per_unit: 3}\n  - key: c\n    rate_limit: {unit: second, requests_per_unit: 1}\n  - key: u\n    rate_limit: {unlimited: true}\n";
    let s = service(cfg).with_response_headers(ResponseHeaders::default());
    let t = T + 30;
    let pairs = |r: &RateLimitResponse| {
        r.response_headers_to_add
            .iter()
            .map(|h| {
                assert!(h.raw_value.is_empty());
                (h.key.clone(), h.value.clone())
            })
            .collect::<Vec<_>>()
    };
    let want = |limit: &str, remaining: &str, reset: &str| {
        vec![
            ("RateLimit-Limit".to_owned(), limit.to_owned()),
            ("RateLimit-Remaining".to_owned(), remaining.to_owned()),
            ("RateLimit-Reset".to_owned(), reset.to_owned()),
        ]
    };
    // b has the least left: 2 of 3, for the rest of the hour.
    let r = s
        .decide_at(
            &request(
                "d",
                vec![
                    desc(&[("u", "1")]),
                    desc(&[("a", "1")]),
                    desc(&[("b", "1")]),
                ],
            ),
            t,
        )
        .unwrap();
    assert_eq!(pairs(&r), want("3", "2", "3570"));
    // c goes over: it is reported however much the others have left.
    let r = s
        .decide_at(&request("d", vec![desc(&[("c", "1")]); 2]), t)
        .unwrap();
    assert!(over(r.overall_code));
    assert_eq!(pairs(&r), want("1", "0", "1"));
    let r = s
        .decide_at(
            &request("d", vec![desc(&[("c", "1")]), desc(&[("b", "2")])]),
            t,
        )
        .unwrap();
    assert_eq!(pairs(&r), want("1", "0", "1"));
    // Nothing limited: no headers.
    let r = s
        .decide_at(
            &request("d", vec![desc(&[("u", "1")]), desc(&[("z", "1")])]),
            t,
        )
        .unwrap();
    assert!(r.response_headers_to_add.is_empty());
    // Names of the operator's choosing.
    let s = service(cfg).with_response_headers(ResponseHeaders {
        limit: "X-RateLimit-Limit".into(),
        remaining: "X-RateLimit-Remaining".into(),
        reset: "X-RateLimit-Reset".into(),
    });
    let r = s
        .decide_at(&request("d", vec![desc(&[("a", "1")])]), t)
        .unwrap();
    let keys: Vec<_> = pairs(&r).into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        keys,
        [
            "X-RateLimit-Limit",
            "X-RateLimit-Remaining",
            "X-RateLimit-Reset"
        ]
    );
}

#[test]
fn limits_beyond_the_wire_types_are_clamped() {
    let s = service(
        "domain: d\ndescriptors:\n  - key: big\n    rate_limit: {unit: second, requests_per_unit: 10000000000}\n",
    );
    let r = s
        .decide_at(&request("d", vec![desc(&[("big", "1")])]), T)
        .unwrap();
    let st = &r.statuses[0];
    assert_eq!(
        st.current_limit.as_ref().unwrap().requests_per_unit,
        u32::MAX
    );
    assert_eq!(st.limit_remaining, u32::MAX);
}

#[test]
fn the_clock_decides_the_window() {
    let now = Arc::new(std::sync::atomic::AtomicU64::new(T));
    let clock = Arc::clone(&now);
    let s = service(BASIC).with_clock(move || clock.load(std::sync::atomic::Ordering::Relaxed));
    let req = request("test-domain", vec![desc(&[("key3", "c")])]);
    assert!(ok(s.decide(&req).unwrap().overall_code));
    assert!(over(s.decide(&req).unwrap().overall_code));
    now.store(T + 3600, std::sync::atomic::Ordering::Relaxed);
    assert!(ok(s.decide(&req).unwrap().overall_code));
    assert_eq!(s.limiter().config().domains().count(), 1);
    let dbg = format!("{s:?}");
    assert!(
        dbg.starts_with("RateLimitService {") && dbg.contains("headers: None"),
        "{dbg}"
    );
    // The system clock is Unix time.
    let sys = RateLimitService::new(Arc::clone(s.limiter()));
    let r = sys.decide(&req).unwrap();
    let left = secs(&r.statuses[0]).unwrap();
    assert!((1..=3600).contains(&left), "{left}");
    assert!(unix_now() > T);
}

#[test]
fn global_shadow_mode_answers_ok_but_keeps_the_statuses() {
    let yaml = "domain: d\ndescriptors:\n  - key: k\n    rate_limit: {unit: minute, requests_per_unit: 1}\n";
    let s = service(yaml).with_global_shadow_mode(true);
    let req = || request("d", vec![desc(&[("k", "v")])]);
    let r = s.decide_at(&req(), T).unwrap();
    assert!(ok(r.overall_code) && ok(r.statuses[0].code));
    let r = s.decide_at(&req(), T).unwrap();
    assert!(ok(r.overall_code), "global shadow mode is OK overall");
    assert!(over(r.statuses[0].code), "the status still says over");
    assert_eq!(r.statuses[0].limit_remaining, 0);
    // Off again, the same counter is over.
    let s = s.with_global_shadow_mode(false);
    assert!(over(s.decide_at(&req(), T).unwrap().overall_code));
}
