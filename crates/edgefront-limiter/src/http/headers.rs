//! `RateLimit-Policy`, `RateLimit` and `Retry-After`, as
//! draft-ietf-httpapi-ratelimit-headers-11 (May 2026) defines the first
//! two and RFC 9110 the third.
//!
//! Both draft fields are Structured Field Lists (RFC 9651) whose items are
//! the policy's name as a String; every rule that applied to a request
//! adds one item to each, so a request under two rules says so. A rule
//! is a GCRA bucket of `burst` tokens refilled one per interval, and maps
//! onto the draft's fields like this:
//!
//! - **`RateLimit-Policy: "login";q=5;w=60`**: `q`, the quota, is the
//!   burst: the most requests the policy allows at once. `w`, the window,
//!   is the time a whole burst takes to refill, in seconds, rounded up
//!   (the draft's `w` is a non-zero integer and only informational). The
//!   unit is the default, requests, so `qu` is left out.
//! - **`RateLimit: "login";r=2;t=11;edgefront-ms=11999`**: `r` is the
//!   tokens left after this request. `t` is the draft's effective window:
//!   the seconds within which the client can use no more than `r`, which
//!   for a GCRA bucket is the time until its next token, rounded *down*
//!   so the statement stays true. A full bucket gains nothing by waiting,
//!   so it has no `t`. Seconds are coarse for a limit of several a second,
//!   so the item also carries `edgefront-ms`, a vendor parameter the
//!   draft allows as a comment: the same wait in milliseconds, rounded
//!   *up*, so a client that waits that long finds the token there.
//! - **`Retry-After: 12`** on a refusal: the seconds until the refused
//!   request would be admitted, from the decision's exact
//!   `retry_after_ns`, rounded up, so it is never early (the draft says it
//!   takes precedence over `t` and must not end before it).

use http::HeaderValue;
use http::header::HeaderName;

use crate::gcra::Rate;

pub const RATELIMIT_POLICY: HeaderName = HeaderName::from_static("ratelimit-policy");
pub const RATELIMIT: HeaderName = HeaderName::from_static("ratelimit");

const NS: u128 = 1_000_000_000;
const MS: u128 = 1_000_000;

/// The `RateLimit-Policy` item for a rule named `name` with `rate`. The
/// name must be one an sf-string holds unescaped, as rule names are.
pub fn policy_item(name: &str, rate: &Rate) -> String {
    format!("\"{name}\";q={};w={}", rate.burst, window_secs(rate))
}

/// The seconds a whole burst takes to refill, rounded up, at least 1.
pub fn window_secs(rate: &Rate) -> u64 {
    let ns = u128::from(rate.burst) * u128::from(rate.period_ns);
    let tokens = u128::from(rate.tokens.max(1));
    let secs = (ns.div_ceil(tokens)).div_ceil(NS);
    u64::try_from(secs.max(1)).unwrap_or(u64::MAX)
}

/// How long until a bucket of `rate` with `remaining` tokens, which will
/// be full in `reset_ns`, has one more token; `None` when it is full.
pub fn next_token_ns(rate: &Rate, remaining: u64, reset_ns: u64) -> Option<u64> {
    if remaining >= rate.burst || reset_ns == 0 {
        return None;
    }
    // The bucket is full after `reset_ns`; it reaches `remaining + 1`
    // tokens that many intervals of refill earlier.
    let missing = u128::from(rate.burst - remaining - 1);
    let early = (missing * u128::from(rate.period_ns)) / u128::from(rate.tokens.max(1));
    let wait = u128::from(reset_ns).saturating_sub(early);
    Some(u64::try_from(wait).unwrap_or(u64::MAX))
}

/// The `RateLimit` item for a rule named `name`.
pub fn limit_item(name: &str, rate: &Rate, remaining: u64, reset_ns: u64) -> String {
    match next_token_ns(rate, remaining, reset_ns) {
        None => format!("\"{name}\";r={remaining}"),
        Some(wait) => {
            let wait = u128::from(wait);
            format!(
                "\"{name}\";r={remaining};t={};edgefront-ms={}",
                wait / NS,
                wait.div_ceil(MS)
            )
        }
    }
}

/// The `RateLimit` item on a refusal: nothing left, and the wait the
/// refused request has before it would be admitted.
pub fn refused_item(name: &str, retry_after_ns: u64) -> String {
    let wait = u128::from(retry_after_ns);
    format!(
        "\"{name}\";r=0;t={};edgefront-ms={}",
        wait / NS,
        wait.div_ceil(MS)
    )
}

/// `Retry-After` in whole seconds for a refusal that would be admitted
/// after `retry_after_ns`, rounded up and at least 1.
pub fn retry_after_secs(retry_after_ns: u64) -> u64 {
    u64::try_from(u128::from(retry_after_ns).div_ceil(NS).max(1)).unwrap_or(u64::MAX)
}

/// The `Retry-After` value.
pub fn retry_after(retry_after_ns: u64) -> HeaderValue {
    HeaderValue::from(retry_after_secs(retry_after_ns))
}

/// Joins items into one list field value. Items are built here from rule
/// names and integers, so the value is always valid.
pub fn list(items: &[String]) -> HeaderValue {
    HeaderValue::from_str(&items.join(", ")).expect("rule names and integers are visible ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{Table, TableConfig};

    const SEC: u64 = 1_000_000_000;

    #[test]
    fn the_policy_states_the_burst_and_its_refill() {
        assert_eq!(
            policy_item("login", &Rate::new(5, 60 * SEC, 5)),
            "\"login\";q=5;w=60"
        );
        assert_eq!(
            policy_item("api", &Rate::new(10, SEC, 20)),
            "\"api\";q=20;w=2"
        );
        // A third of a second rounds up to the draft's smallest window.
        assert_eq!(window_secs(&Rate::new(30, SEC, 10)), 1);
        // 7 per 10 s with a burst of 3 refills in 4.29 s: 5.
        assert_eq!(window_secs(&Rate::new(7, 10 * SEC, 3)), 5);
    }

    #[test]
    fn retry_after_rounds_up_and_is_never_zero() {
        assert_eq!(retry_after_secs(1), 1);
        assert_eq!(retry_after_secs(SEC), 1);
        assert_eq!(retry_after_secs(SEC + 1), 2);
        assert_eq!(retry_after_secs(0), 1);
        assert_eq!(retry_after(11_200_000_000), "12");
        assert_eq!(retry_after_secs(u64::MAX), 18_446_744_074);
    }

    /// Every header against the table's exact decisions: a client that
    /// waits what `edgefront-ms` or `Retry-After` says is admitted, and
    /// one that comes back a millisecond sooner is not.
    #[test]
    fn headers_match_exact_gcra_retry_times() {
        for rate in [
            Rate::new(5, 60 * SEC, 5),
            Rate::new(10, SEC, 20),
            Rate::new(3, 7 * SEC, 2),
            Rate::new(1000, SEC, 50),
        ] {
            let t = Table::new(&TableConfig::new(64, rate)).unwrap();
            let start = 1_000 * SEC;
            for now in (start..).take(rate.burst as usize) {
                let (d, reset) = t.take_reset(b"k", now, 1).unwrap();
                assert!(d.allowed);
                let item = limit_item("p", &rate, d.remaining, reset);
                if d.remaining == 0 {
                    // The wait for the next token is the refusal's retry.
                    let (refused, _) = t.take_reset(b"k", now, 1).unwrap();
                    assert!(!refused.allowed);
                    let wait = next_token_ns(&rate, 0, reset).unwrap();
                    assert!(
                        wait.abs_diff(refused.retry_after_ns) <= 1,
                        "{wait} vs {refused:?}"
                    );
                    let ms = wait.div_ceil(1_000_000);
                    assert!(
                        item.ends_with(&format!(";t={};edgefront-ms={ms}", wait / SEC)),
                        "{item}"
                    );
                    let ms = refused.retry_after_ns.div_ceil(1_000_000);
                    assert_eq!(
                        retry_after_secs(refused.retry_after_ns),
                        refused.retry_after_ns.div_ceil(SEC)
                    );
                    // A millisecond early: still refused, and the table
                    // is left as it was.
                    let early = now + ms * 1_000_000 - 1_000_000;
                    assert!(!t.take(b"k", early, 1).unwrap().allowed, "{rate:?}");
                    // On the millisecond: admitted.
                    assert!(t.take(b"k", now + ms * 1_000_000, 1).unwrap().allowed);
                    break;
                }
            }
            // A fresh key's bucket is full: no window.
            let (d, reset) = t.take_reset(b"fresh", start, 0).unwrap();
            assert_eq!(reset, 0);
            assert_eq!(
                limit_item("p", &rate, d.remaining, reset),
                format!("\"p\";r={}", rate.burst)
            );
        }
    }

    #[test]
    fn the_effective_window_is_the_wait_for_the_next_token() {
        // 1 per second, burst 5, two taken: full in 2 s, a token in 1 s.
        let rate = Rate::new(1, SEC, 5);
        assert_eq!(next_token_ns(&rate, 3, 2 * SEC), Some(SEC));
        assert_eq!(
            limit_item("p", &rate, 3, 2 * SEC),
            "\"p\";r=3;t=1;edgefront-ms=1000"
        );
        // 1.5 s to the next token: t rounds down, the milliseconds up.
        assert_eq!(
            limit_item("p", &rate, 3, 2_500_000_001),
            "\"p\";r=3;t=1;edgefront-ms=1501"
        );
        assert_eq!(next_token_ns(&rate, 5, 0), None);
        assert_eq!(
            list(&["\"a\";r=1".into(), "\"b\";r=2".into()]),
            "\"a\";r=1, \"b\";r=2"
        );
    }
}
