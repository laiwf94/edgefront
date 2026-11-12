//! Property tests for `RateSpec`: every accepted rate is held as the exact
//! fraction written, is enforced within 0.1% of it and never faster, and
//! long windows become quotas that hold every window to the count. The
//! checks here recompute everything from the text with plain integer
//! arithmetic, not through the crate's own helpers.

use edgefront_limiter::gcra::TICK_SHIFT;
use edgefront_limiter::rate::{MAX_PERIOD_NS, QUOTA_MIN_PERIOD_NS};
use edgefront_limiter::{Gcra, Plan, RateSpec};
use proptest::prelude::*;

const SEC: u64 = 1_000_000_000;
const DAY: u64 = 86_400 * SEC;

const UNITS: [(&str, u64); 12] = [
    ("s", SEC),
    ("sec", SEC),
    ("seconds", SEC),
    ("m", 60 * SEC),
    ("min", 60 * SEC),
    ("minutes", 60 * SEC),
    ("h", 3_600 * SEC),
    ("hr", 3_600 * SEC),
    ("hours", 3_600 * SEC),
    ("d", DAY),
    ("day", DAY),
    ("days", DAY),
];

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// A rate's text and what it says: count = mantissa / 10^scale per
/// period_ns, and the burst if written.
#[derive(Clone, Debug)]
struct Written {
    text: String,
    mantissa: u128,
    scale: u32,
    period_ns: u128,
    burst: Option<u64>,
}

fn written() -> impl Strategy<Value = Written> {
    (
        0u64..2_000_000,
        0u32..=9,
        0u64..1_000_000_000,
        prop_oneof![Just(None), (1u64..400).prop_map(Some)],
        0..UNITS.len(),
        prop_oneof![3 => Just(None), 1 => (1u64..1_000_000).prop_map(Some)],
    )
        .prop_map(|(whole, scale, frac, every, unit, burst)| {
            let frac = frac % 10u64.pow(scale);
            let (name, unit_ns) = UNITS[unit];
            let mut text = whole.to_string();
            if scale > 0 {
                text.push_str(&format!(".{frac:0w$}", w = scale as usize));
            }
            text.push('/');
            if let Some(e) = every {
                text.push_str(&e.to_string());
            }
            text.push_str(name);
            if let Some(b) = burst {
                text.push_str(&format!(" burst {b}"));
            }
            Written {
                text,
                mantissa: u128::from(whole) * 10u128.pow(scale) + u128::from(frac),
                scale,
                period_ns: u128::from(every.unwrap_or(1)) * u128::from(unit_ns),
                burst,
            }
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn accepted_rates_are_exact_and_within_a_thousandth(w in written()) {
        let pow = 10u128.pow(w.scale);
        let parsed = RateSpec::parse(&w.text);
        let r = match parsed {
            Ok(r) => r,
            Err(e) => {
                // Only the documented refusals, each of them for a reason
                // that holds.
                let m = e.message;
                if w.mantissa == 0 {
                    prop_assert!(m.ends_with("a rate must be above zero"), "{}", m);
                } else if w.period_ns > u128::from(MAX_PERIOD_NS) {
                    prop_assert!(m.ends_with("the period is longer than 366 days"), "{}", m);
                } else {
                    let k = pow / gcd(w.mantissa, pow);
                    let quota = w.burst.is_none() && w.period_ns >= u128::from(QUOTA_MIN_PERIOD_NS);
                    // The bucket's exact rate, reduced.
                    let g = gcd(w.mantissa, w.period_ns * pow);
                    let (tokens, period) = (w.mantissa / g, w.period_ns * pow / g);
                    let iv = (period << TICK_SHIFT).div_ceil(tokens);
                    let burst = w.burst.map_or(w.mantissa.div_ceil(pow).max(1), u128::from);
                    let why = if quota && w.period_ns * k > u128::from(MAX_PERIOD_NS) {
                        "below one per 366 days"
                    } else if quota {
                        "a quota is refused only for its window"
                    } else if period > tokens * u128::from(MAX_PERIOD_NS) {
                        "below one per 366 days"
                    } else if u64::try_from(period).is_err() {
                        "too precise"
                    } else if iv * burst > 1 << 62 {
                        "rate and burst are too large together"
                    } else if (iv * tokens - (period << TICK_SHIFT)) * 1_000 > period << TICK_SHIFT {
                        "too fast to enforce within 0.1%"
                    } else {
                        "nothing"
                    };
                    prop_assert!(m.contains(why), "{}: refused with {} but expected {}", w.text, m, why);
                }
                return Ok(());
            }
        };
        let (num, den) = r.count();
        prop_assert_eq!(num * pow, w.mantissa * den);
        prop_assert_eq!(u128::from(r.period_ns()), w.period_ns);
        let burst = w.burst.unwrap_or_else(|| w.mantissa.div_ceil(pow).max(1) as u64);
        prop_assert_eq!(r.burst(), burst);
        match r.plan() {
            Plan::Quota { limit, window_ns } => {
                prop_assert!(w.burst.is_none());
                prop_assert!(w.period_ns >= u128::from(QUOTA_MIN_PERIOD_NS));
                let (limit, window) = (u128::from(limit), u128::from(window_ns));
                // limit / window == count / period, exactly.
                prop_assert_eq!(limit * w.period_ns * pow, w.mantissa * window);
                // The window is the shortest whole multiple of the period
                // that holds a whole count.
                prop_assert_eq!(window % w.period_ns, 0);
                prop_assert_eq!(gcd(limit, window / w.period_ns), 1);
                prop_assert!(window <= u128::from(MAX_PERIOD_NS));
            }
            Plan::Bucket(rate) => {
                prop_assert!(w.burst.is_some() || w.period_ns < u128::from(QUOTA_MIN_PERIOD_NS));
                prop_assert_eq!(rate.burst, burst);
                // tokens / period == count / written period, exactly.
                prop_assert_eq!(
                    u128::from(rate.tokens) * w.period_ns * pow,
                    w.mantissa * u128::from(rate.period_ns)
                );
                // The limiter's time per token, in ticks, is never shorter
                // than written and longer by at most a thousandth.
                let g = Gcra::new(&rate).unwrap();
                let written = u128::from(rate.period_ns) << TICK_SHIFT;
                let enforced = u128::from(g.interval()) * u128::from(rate.tokens);
                prop_assert!(enforced >= written);
                prop_assert!((enforced - written) * 1_000 <= written, "{}", w.text);
            }
        }
    }

    #[test]
    fn display_parses_back_to_the_same_rate(w in written()) {
        if let Ok(r) = RateSpec::parse(&w.text) {
            let shown = r.to_string();
            prop_assert_eq!(RateSpec::parse(&shown).unwrap(), r);
            prop_assert_eq!(RateSpec::parse(&shown).unwrap().to_string(), shown);
        }
    }

    #[test]
    fn random_text_never_panics(s in "[0-9./a-z kmg]{0,24}") {
        let _ = RateSpec::parse(&s);
    }
}

/// A greedy client: one at a time from `start` until `end`, coming back
/// exactly when told to after a denial. Returns the grant times.
fn greedy(r: &RateSpec, start: u64, end: u64) -> Vec<u64> {
    let mut l = r.plan().limiter(start);
    let mut grants = Vec::new();
    let mut now = start;
    while now < end {
        let d = l.take(now, 1).unwrap();
        if d.allowed {
            grants.push(now);
        } else {
            assert!(d.retry_after_ns > 0, "{r}: denied with no wait");
            now += d.retry_after_ns;
        }
    }
    grants
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Over a simulated day a bucket grants its burst plus the rate's
    /// share of the day, within a thousandth (or one request) and never
    /// more.
    #[test]
    fn buckets_hold_the_rate_over_a_simulated_day(
        count in 1u64..20_000,
        scale in 0u32..=3,
        every in 1u64..=59,
        unit in prop_oneof![Just(("s", SEC)), Just(("min", 60 * SEC))],
        start in 0u64..1 << 50,
    ) {
        let pow = 10u64.pow(scale);
        let text = format!("{}.{:0w$}/{every}{}", count / pow, count % pow, unit.0, w = scale as usize);
        let text = text.replace("./", "/");
        let r = RateSpec::parse(&text).unwrap();
        let period = u128::from(every * unit.1);
        // Keep the simulation short: at most 300,000 grants in the day.
        let (num, den) = r.count();
        let target_num = num * u128::from(DAY);
        let target_den = den * period;
        prop_assume!(target_num / target_den <= 300_000);
        let grants = greedy(&r, start, start + DAY).len() as u128;
        let refilled = grants - u128::from(r.burst());
        // refilled vs target = target_num / target_den, in whole numbers.
        prop_assert!(refilled * target_den <= target_num + target_den, "{}: {} too many", text, refilled);
        let short = (target_num).saturating_sub(refilled * target_den);
        prop_assert!(
            short <= target_den || short * 1_000 <= target_num,
            "{}: {} refilled of {}", text, refilled, target_num as f64 / target_den as f64
        );
    }

    /// A quota grants exactly its limit in its first window, never more
    /// than the limit plus one bucket's worth in any window, and keeps
    /// granting the limit per window as it slides.
    #[test]
    fn quotas_hold_every_window_to_the_count(
        limit in 1u64..5_000,
        every in 1u64..=48,
        unit in prop_oneof![Just(("h", 3_600 * SEC)), Just(("day", DAY))],
        start in 0u64..1 << 50,
    ) {
        let text = format!("{limit}/{every}{}", unit.0);
        let r = RateSpec::parse(&text).unwrap();
        let Plan::Quota { limit: l, window_ns } = r.plan() else {
            panic!("{text} is not a quota")
        };
        prop_assert_eq!((l, window_ns), (limit, every * unit.1));
        let windows = 4;
        let grants = greedy(&r, start, start + windows * window_ns);
        let in_window = |from: u64| {
            let lo = grants.partition_point(|&t| t < from);
            let hi = grants.partition_point(|&t| t < from + window_ns);
            (hi - lo) as u64
        };
        prop_assert_eq!(in_window(start), limit);
        for i in 1..windows {
            let got = in_window(start + i * window_ns);
            prop_assert!(got.abs_diff(limit) * 1_000 <= limit || got.abs_diff(limit) <= 1,
                "{}: window {} granted {}", text, i, got);
        }
        // Sliding: any window starting at a grant holds at most the limit
        // plus the share of one bucket (a sixteenth).
        let slack = limit.div_ceil(16);
        for &t in grants.iter().step_by((grants.len() / 64).max(1)) {
            prop_assert!(in_window(t) <= limit + slack, "{}: {} from {}", text, in_window(t), t);
        }
    }
}
