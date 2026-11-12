use super::*;

const MIN: u64 = 60 * SEC;
const HOUR: u64 = 3_600 * SEC;
const DAY: u64 = 86_400 * SEC;

fn spec(s: &str) -> RateSpec {
    RateSpec::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

fn plan(s: &str) -> Plan {
    spec(s).plan()
}

fn message(s: &str) -> String {
    RateSpec::parse(s).unwrap_err().message
}

#[test]
fn short_periods_are_exact_token_buckets() {
    assert_eq!(plan("0.5/s"), Plan::Bucket(Rate::new(1, 2 * SEC, 1)));
    assert_eq!(plan("90/min"), Plan::Bucket(Rate::new(3, 2 * SEC, 90)));
    assert_eq!(plan("10/s"), Plan::Bucket(Rate::new(1, SEC / 10, 10)));
    assert_eq!(plan("0.1/s"), Plan::Bucket(Rate::new(1, 10 * SEC, 1)));
    assert_eq!(plan("2.5/10s"), Plan::Bucket(Rate::new(1, 4 * SEC, 3)));
    assert_eq!(
        plan("2.5/10s burst 5"),
        Plan::Bucket(Rate::new(1, 4 * SEC, 5))
    );
    assert_eq!(plan("100/10s"), Plan::Bucket(Rate::new(1, SEC / 10, 100)));
    assert_eq!(plan("1/59m"), Plan::Bucket(Rate::new(1, 59 * MIN, 1)));
    // pacer's size multipliers still mean powers of 1024.
    assert_eq!(
        plan("1m/s"),
        Plan::Bucket(Rate::new(1 << 11, 1_953_125, 1 << 20))
    );
    assert_eq!(plan("1.5k/s"), Plan::Bucket(Rate::new(3, 1_953_125, 1536)));
    assert_eq!(
        plan("0.0001/s"),
        Plan::Bucket(Rate::new(1, 10_000 * SEC, 1))
    );
}

#[test]
fn hours_and_days_without_a_burst_are_quotas() {
    assert_eq!(
        plan("10000/day"),
        Plan::Quota {
            limit: 10_000,
            window_ns: DAY
        }
    );
    assert_eq!(
        plan("100/h"),
        Plan::Quota {
            limit: 100,
            window_ns: HOUR
        }
    );
    assert_eq!(
        plan("60/min"),
        Plan::Bucket(Rate::new(1, SEC, 60)),
        "a minute is not a long window"
    );
    assert_eq!(
        plan("1.5/h"),
        Plan::Quota {
            limit: 3,
            window_ns: 2 * HOUR
        }
    );
    assert_eq!(
        plan("0.5/day"),
        Plan::Quota {
            limit: 1,
            window_ns: 2 * DAY
        }
    );
    assert_eq!(
        plan("2.25/7d"),
        Plan::Quota {
            limit: 9,
            window_ns: 28 * DAY
        }
    );
    assert_eq!(
        plan("60/60min"),
        Plan::Quota {
            limit: 60,
            window_ns: HOUR
        }
    );
    // With a burst, a long period is a token bucket again.
    assert_eq!(
        plan("10000/day burst 100"),
        Plan::Bucket(Rate::new(1, 8_640_000_000, 100))
    );
}

#[test]
fn units_have_every_spelling() {
    for (text, ns) in [
        ("s", SEC),
        ("sec", SEC),
        ("secs", SEC),
        ("second", SEC),
        ("seconds", SEC),
        ("m", MIN),
        ("min", MIN),
        ("mins", MIN),
        ("minute", MIN),
        ("minutes", MIN),
        ("h", HOUR),
        ("hr", HOUR),
        ("hrs", HOUR),
        ("hour", HOUR),
        ("hours", HOUR),
        ("d", DAY),
        ("day", DAY),
        ("days", DAY),
    ] {
        let r = spec(&format!("1/{text}"));
        assert_eq!(r.period_ns(), ns, "{text}");
        assert_eq!(r.unit().ns(), ns, "{text}");
        assert_eq!(spec(&format!("1/3{text}")).period_ns(), 3 * ns, "{text}");
    }
}

#[test]
fn display_is_canonical_and_parses_back() {
    for (text, shown) in [
        ("0.5/s", "0.5/s"),
        ("0.50/sec", "0.5/s"),
        ("90/m", "90/min"),
        ("10000/days", "10000/day"),
        ("007/1h", "7/h"),
        ("2.5/10s  burst\t5", "2.5/10s burst 5"),
        ("1k/s burst 2k", "1024/s burst 2048"),
        ("1.000000001/s", "1.000000001/s"),
        ("3.0/2hours", "3/2h"),
    ] {
        let r = spec(text);
        assert_eq!(r.to_string(), shown, "{text}");
        assert_eq!(spec(shown), r, "{text}");
    }
}

#[test]
fn accessors_describe_the_rate() {
    let r = spec("2.5/10s");
    assert_eq!(r.count(), (25, 10));
    assert_eq!(r.period_ns(), 10 * SEC);
    assert_eq!(r.unit(), Unit::Second);
    assert_eq!(r.burst(), 3);
    assert!(!r.has_burst());
    assert!((r.per_second() - 0.25).abs() < 1e-12);
    let r = spec("0.001/s burst 7");
    assert_eq!(r.burst(), 7);
    assert!(r.has_burst());
    assert_eq!(spec("0.001/s").burst(), 1);
    assert_eq!(Unit::Day.as_str(), "day");
}

#[test]
fn bad_rates_are_refused_with_the_reason() {
    let like = "expected a rate like 0.5/s, 90/min or 10000/day, got";
    for text in [
        "10",
        "/s",
        "10/",
        "10/w",
        "10/0s",
        "1.5.2/s",
        ".5/s",
        "5./s",
        "-1/s",
        "1e3/s",
        "10/s/s",
        "1t/s",
        "10/1234567890s",
        "10 /s",
    ] {
        let m = message(text);
        assert!(
            m.starts_with(like) || m.starts_with("expected RATE"),
            "{text}: {m}"
        );
    }
    assert_eq!(message("10/w"), format!("{like} \"10/w\""));
    assert_eq!(message(""), format!("{like} nothing"));
    assert_eq!(message("  \t "), format!("{like} nothing"));
    assert_eq!(
        message("10/s burst"),
        "expected RATE or RATE burst N, like 10/s burst 20, got \"10/s burst\""
    );
    assert_eq!(
        message("10/s bust 5"),
        "expected RATE or RATE burst N, like 10/s burst 20, got \"10/s bust 5\""
    );
    for b in ["0", "1.5", "x", "2000000000000000", "1.0"] {
        assert_eq!(
            message(&format!("10/s burst {b}")),
            format!("expected a burst of 1 to 2^50 like 20 or 64k, got {b:?}")
        );
    }
    assert_eq!(message("0/s"), "\"0/s\": a rate must be above zero");
    assert_eq!(message("0.000/s"), "\"0.000/s\": a rate must be above zero");
    assert_eq!(
        message("0.0000000001/s"),
        "\"0.0000000001/s\": a count may have at most 9 decimal places"
    );
    assert_eq!(
        message("1125899906842625/s"),
        "\"1125899906842625/s\": the count is above 2^50"
    );
    assert_eq!(
        message("123456789012345678901/s"),
        "\"123456789012345678901/s\": the count is above 2^50"
    );
    assert_eq!(
        message("2g/s"),
        "\"2g/s\": too fast to enforce within 0.1% (it would run 0.66% slow)"
    );
    assert_eq!(
        message("1/367d"),
        "\"1/367d\": the period is longer than 366 days"
    );
    assert_eq!(
        message("0.001/d"),
        "\"0.001/d\": below one per 366 days: it needs a window of 1000 days"
    );
    assert_eq!(
        message("0.000000001/s"),
        "\"0.000000001/s\": below one per 366 days"
    );
    assert_eq!(
        message("1/365d burst 1000000000"),
        "\"1/365d\": rate and burst are too large together"
    );
    assert_eq!(
        message("0.123456789/59min"),
        "\"0.123456789/59min\": too precise to hold exactly for its period; use fewer decimal places"
    );
    // Long words are cut in messages.
    let long = format!("{}/s", "9".repeat(60));
    assert_eq!(
        message(&long),
        format!("{:?}...: the count is above 2^50", &long[..44])
    );
    let m = message(&format!("{}é/s", "1".repeat(43)));
    assert!(m.ends_with("..."), "{m}");
}

#[test]
fn every_accepted_bucket_is_within_a_thousandth() {
    // Fast enough that the interval rounds noticeably, yet accepted.
    let r = spec("60000000/s");
    let Plan::Bucket(rate) = r.plan() else {
        panic!("bucket")
    };
    let (slow, of) = bucket_slowdown(&rate).unwrap();
    assert!(slow > 0 && slow * MAX_ERROR_DEN <= of, "{slow}/{of}");
    assert_eq!(bucket_slowdown(&Rate::new(0, SEC, 1)), None);
    assert_eq!(
        bucket_slowdown(&Rate::new(1, SEC, 1)),
        Some((0, 64 * SEC as u128))
    );
}

#[test]
fn rates_read_from_toml_with_the_message_on_failure() {
    #[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq)]
    struct Doc {
        api: RateSpec,
    }
    let doc: Doc = toml::from_str("api = \"10000/day\"").unwrap();
    assert_eq!(doc.api, spec("10000/day"));
    assert_eq!(toml::to_string(&doc).unwrap(), "api = \"10000/day\"\n");
    let e = toml::from_str::<Doc>("api = \"10/w\"").unwrap_err();
    assert!(
        e.to_string()
            .contains("expected a rate like 0.5/s, 90/min or 10000/day, got \"10/w\""),
        "{e}"
    );
    let e = toml::from_str::<Doc>("api = 10").unwrap_err();
    assert!(e.to_string().contains("a rate like 0.5/s"), "{e}");
    assert_eq!("0.5/s".parse::<RateSpec>().unwrap(), spec("0.5/s"));
    let e = RateSpec::parse("x").unwrap_err();
    assert_eq!(e.to_string(), e.message);
}

#[test]
fn a_quota_grants_up_to_its_limit_and_says_when_to_retry() {
    let mut q = Quota::new(10, 16 * SEC, 0).unwrap();
    assert_eq!((q.limit(), q.window_ns()), (10, 16 * SEC));
    let d = q.take(0, 4).unwrap();
    assert_eq!(
        (d.allowed, d.granted, d.remaining, d.retry_after_ns),
        (true, 4, 6, 0)
    );
    assert!(q.take(SEC, 6).unwrap().allowed);
    let d = q.take(2 * SEC, 1).unwrap();
    assert_eq!((d.allowed, d.granted, d.remaining), (false, 0, 0));
    // The 4 from t=0 sit in the first bucket, which slides out between 16 s
    // and 17 s, weighted down from 4: it counts 3 from the first nanosecond.
    assert_eq!(d.retry_after_ns, 16 * SEC + 1 - 2 * SEC);
    assert!(!q.take(2 * SEC + d.retry_after_ns - 1, 1).unwrap().allowed);
    assert!(q.take(2 * SEC + d.retry_after_ns, 1).unwrap().allowed);
    assert_eq!(q.take(0, 11), Err(Error::CostExceedsBurst));
    assert_eq!(q.take(100 * SEC, 0).unwrap().granted, 0);
    assert_eq!(q.remaining(100 * SEC), 10);
    assert_eq!(Quota::new(0, SEC, 0), Err(Error::InvalidRate));
    assert_eq!(Quota::new(1, 15, 0), Err(Error::InvalidWindow));
}

#[test]
fn limiters_follow_their_plan() {
    let mut l = plan("2/s").limiter(0);
    assert!(matches!(l, Limiter::Bucket(_)));
    assert_eq!(l.remaining(0), 2);
    assert!(l.take(0, 2).unwrap().allowed);
    assert_eq!(l.take(0, 1).unwrap().retry_after_ns, SEC / 2);
    let mut l = plan("2/h").limiter(5);
    assert!(matches!(l, Limiter::Quota(_)));
    assert_eq!(l.remaining(5), 2);
    assert!(l.take(5, 2).unwrap().allowed);
    assert!(!l.take(HOUR, 1).unwrap().allowed);
    assert_eq!(l.remaining(HOUR), 0);
}

/// A greedy client from `start` for `days` days: it takes one at a time
/// and, when denied, comes back exactly when told to. How many were
/// granted in each day.
fn saturate(text: &str, start: u64, days: u64) -> Vec<u64> {
    let mut l = plan(text).limiter(start);
    let mut per_day = vec![0; days as usize];
    let mut now = start;
    while now < start + days * DAY {
        let d = l.take(now, 1).unwrap();
        if d.allowed {
            per_day[((now - start) / DAY) as usize] += 1;
        } else {
            assert!(d.retry_after_ns > 0, "{text}: denied at {now} with no wait");
            now += d.retry_after_ns;
        }
    }
    per_day
}

fn within_a_thousandth(got: u64, want: u64) -> bool {
    got.abs_diff(want) * 1_000 <= want
}

#[test]
fn the_roadmap_rates_hold_within_a_thousandth_over_a_simulated_day() {
    // From a start that is not aligned to anything.
    let start = 12_345_678_901;
    for (text, want) in [
        ("0.5/s", 43_200),
        ("90/min", 129_600),
        ("10000/day", 10_000),
    ] {
        let got = saturate(text, start, 1)[0];
        assert!(within_a_thousandth(got, want), "{text}: {got} of {want}");
    }
    // Ten thousand a day also holds every day after the first, as the
    // window slides.
    let days = saturate("10000/day", start, 8);
    for (day, got) in days.into_iter().enumerate() {
        assert!(within_a_thousandth(got, 10_000), "day {day}: {got}");
    }
}
