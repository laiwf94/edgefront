//! Retries: steer's `internal/retry` tests translated (the budget's floor
//! and ratio, a zero budget, the reference model and the backoff curve),
//! plus the retry decisions from its `transport.go`.
//!
//! Added here:
//! - the property that retries stay within the budget for any trace of
//!   requests, failures and clock jumps, checked against an independent
//!   log of every request and retry, together with `max_retries` per
//!   request and the retriable outcomes;
//! - the budget against the reference model under proptest, so a failure
//!   shrinks to a short script;
//! - concurrent withdrawals never taking more than the budget holds;
//! - the per-try timeout on a manual clock.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use edgefront_core::clock::{Clock, ManualClock};
use edgefront_core::config;
use edgefront_core::http1::ClientError;
use edgefront_proxy::balancer::rng::{Rng, SplitMix64};
use edgefront_proxy::retry::{
    Budget, Outcome, PerTryTimeout, Policy, backoff, idempotent, per_try,
};
use http::{HeaderMap, HeaderValue, Method};
use proptest::prelude::*;

fn budget_cfg(ratio: f64, min_per_second: u32, window: Duration) -> config::Budget {
    config::Budget {
        ratio,
        min_per_second,
        window,
    }
}

fn retry_cfg() -> config::Retry {
    config::Retry {
        max_retries: 2,
        retry_on: vec![502, 503, 504],
        per_try_timeout: Duration::ZERO,
        backoff_base: Duration::from_millis(25),
        backoff_max: Duration::from_millis(250),
        max_body_bytes: 64 << 10,
        budget: budget_cfg(0.2, 10, Duration::from_secs(10)),
    }
}

/// A uniform draw in `[0, 1)` from SplitMix64, as health checks draw
/// their jitter.
fn unit(rng: &SplitMix64) -> f64 {
    (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
}

// ---- steer's internal/retry tests ----

#[test]
fn budget_floor_and_ratio() {
    let clock = Arc::new(ManualClock::new());
    let b = Budget::new(&budget_cfg(0.2, 1, Duration::from_secs(10)), clock.clone());
    // Floor: 1/s × 10 s = 10 retries with no traffic at all.
    for i in 0..10 {
        assert!(b.try_withdraw(), "floor retry {i} refused");
    }
    assert!(!b.try_withdraw(), "retry beyond the floor allowed");
    // 100 requests buy 20 more.
    for _ in 0..100 {
        b.deposit();
    }
    assert_eq!(b.balance(), 20.0);
    for i in 0..20 {
        assert!(b.try_withdraw(), "ratio retry {i} refused");
    }
    assert!(!b.try_withdraw(), "retry beyond the ratio allowed");
    assert_eq!(b.totals(), (100, 30));
    // Everything ages out of the window.
    clock.advance(Duration::from_secs(10));
    assert_eq!(b.balance(), 10.0);
    assert_eq!(b.totals(), (0, 0));
}

#[test]
fn budget_zero() {
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new());
    let b = Budget::new(&budget_cfg(0.0, 0, Duration::from_secs(1)), clock.clone());
    b.deposit();
    assert!(!b.try_withdraw(), "zero budget allowed a retry");
    assert_eq!(b.balance(), 0.0);
    let short = Budget::new(&budget_cfg(0.0, 1, Duration::ZERO), clock.clone());
    assert_eq!(
        short.window_seconds(),
        1,
        "a window under a second rounds up"
    );
    assert_eq!(short.balance(), 1.0);
    let ragged = Budget::new(&budget_cfg(0.0, 1, Duration::from_millis(2900)), clock);
    assert_eq!(
        ragged.window_seconds(),
        2,
        "a window rounds down to seconds"
    );
}

#[test]
fn balance_can_be_fractional() {
    let clock = Arc::new(ManualClock::new());
    let b = Budget::new(&budget_cfg(0.2, 0, Duration::from_secs(10)), clock);
    for _ in 0..3 {
        b.deposit();
    }
    assert!((b.balance() - 0.6).abs() < 1e-12);
    assert!(b.try_withdraw(), "0.6 left allows a retry");
    assert_eq!(b.balance(), 0.0);
    assert!(!b.try_withdraw());
}

#[test]
fn the_window_slides_a_second_at_a_time() {
    let clock = Arc::new(ManualClock::new());
    let b = Budget::new(&budget_cfg(1.0, 0, Duration::from_secs(3)), clock.clone());
    for sec in 0..3 {
        b.deposit();
        assert_eq!(b.totals(), (sec + 1, 0), "second {sec}");
        clock.advance(Duration::from_secs(1));
    }
    // Second 3: second 0 has left the window.
    assert_eq!(b.totals(), (2, 0));
    clock.advance(Duration::from_millis(999));
    assert_eq!(b.totals(), (2, 0), "still second 3");
    clock.advance(Duration::from_millis(1));
    assert_eq!(b.totals(), (1, 0), "second 4");
}

/// One event in the reference: the second it happened in, and whether it
/// was a retry.
#[derive(Debug, Clone, Copy)]
struct Event {
    sec: u64,
    retry: bool,
}

/// The reference keeps every event and counts the ones whose second lies
/// inside the window: steer's `TestBudgetMatchesReference`.
struct Reference {
    window: u64,
    ratio: f64,
    min_per_second: u64,
    events: Vec<Event>,
}

impl Reference {
    fn counts(&self, now: u64) -> (f64, f64) {
        let mut requests = 0.0;
        let mut retries = 0.0;
        for e in &self.events {
            if e.sec + self.window > now {
                if e.retry {
                    retries += 1.0;
                } else {
                    requests += 1.0;
                }
            }
        }
        (requests, retries)
    }

    fn allowed(&self, requests: f64) -> f64 {
        (self.min_per_second * self.window) as f64 + self.ratio * requests
    }

    fn may_withdraw(&self, now: u64) -> bool {
        let (requests, retries) = self.counts(now);
        retries < self.allowed(requests)
    }

    fn balance(&self, now: u64) -> f64 {
        let (requests, retries) = self.counts(now);
        (self.allowed(requests) - retries).max(0.0)
    }
}

/// One step of a budget script.
#[derive(Debug, Clone, Copy)]
enum Op {
    Deposit,
    Withdraw,
    Advance(u64),
}

/// Runs `ops` against a budget and the reference, failing at the first
/// step where they disagree on a decision or on the balance.
fn check_against_reference(
    window: u64,
    ratio: f64,
    min_per_second: u32,
    offset_ms: u64,
    ops: &[Op],
) -> Result<(), String> {
    let clock = Arc::new(ManualClock::new());
    let origin = clock.now();
    // steer's fake clock starts part way into a second.
    clock.advance(Duration::from_millis(offset_ms));
    let b = Budget::with_origin(
        &budget_cfg(ratio, min_per_second, Duration::from_secs(window)),
        clock.clone(),
        origin,
    );
    let mut reference = Reference {
        window,
        ratio,
        min_per_second: min_per_second.into(),
        events: Vec::new(),
    };
    for (step, op) in ops.iter().enumerate() {
        let now = (clock.now() - origin).as_secs();
        match *op {
            Op::Deposit => {
                b.deposit();
                reference.events.push(Event {
                    sec: now,
                    retry: false,
                });
            }
            Op::Withdraw => {
                let want = reference.may_withdraw(now);
                let got = b.try_withdraw();
                if got != want {
                    return Err(format!("step {step}: try_withdraw {got}, reference {want}"));
                }
                if want {
                    reference.events.push(Event {
                        sec: now,
                        retry: true,
                    });
                }
            }
            Op::Advance(ms) => {
                clock.advance(Duration::from_millis(ms));
            }
        }
        let now = (clock.now() - origin).as_secs();
        let (got, want) = (b.balance(), reference.balance(now));
        if got != want {
            return Err(format!("step {step}: balance {got}, reference {want}"));
        }
    }
    Ok(())
}

#[test]
fn budget_matches_reference() {
    let rng = SplitMix64::new(0x5eed_0508);
    for iter in 0..200 {
        let window = 1 + rng.below(10) as u64;
        let ratio = rng.below(5) as f64 / 10.0;
        let min_per_second = rng.below(3) as u32;
        let offset_ms = rng.below(1000) as u64;
        let ops: Vec<Op> = (0..2000)
            .map(|_| match rng.below(10) {
                0..=4 => Op::Deposit,
                5..=8 => Op::Withdraw,
                _ => Op::Advance(rng.below(2500) as u64),
            })
            .collect();
        if let Err(e) = check_against_reference(window, ratio, min_per_second, offset_ms, &ops) {
            panic!("iter {iter} (window {window}, ratio {ratio}, min {min_per_second}): {e}");
        }
    }
}

#[test]
fn backoff_curve() {
    let top = 0.999_999;
    let base = Duration::from_millis(25);
    let cap = Duration::from_millis(250);
    let ms = Duration::from_millis;
    for (attempt, want) in [
        (0, ms(25)),
        (1, ms(50)),
        (2, ms(100)),
        (3, ms(200)),
        (4, ms(250)),
        (40, ms(250)),
        (62, ms(250)),
        (u32::MAX, ms(250)),
    ] {
        let got = backoff(attempt, base, cap, top);
        assert!(
            got <= want && got >= want * 99 / 100,
            "attempt {attempt}: {got:?} want just under {want:?}"
        );
    }
    assert_eq!(
        backoff(3, Duration::from_millis(1), Duration::from_secs(1), 0.0),
        Duration::ZERO,
        "full jitter must reach zero"
    );
}

#[test]
fn backoff_caps_a_doubling_that_wraps() {
    // 2^40 ns doubled 31 times wraps i64 to zero, and 2^33 ns doubled 30
    // times is exactly i64's sign bit: both fall to the cap.
    let cap = Duration::from_secs(60);
    assert_eq!(
        backoff(31, Duration::from_nanos(1 << 40), cap, 0.5),
        Duration::from_secs(30)
    );
    assert_eq!(
        backoff(30, Duration::from_nanos(1 << 33), cap, 0.5),
        Duration::from_secs(30)
    );
    // A doubling that wraps to a small positive value would sit below the
    // cap; steer takes it, and so does the port.
    let base = Duration::from_nanos((1 << 62) + 3);
    assert_eq!(
        backoff(2, base, cap, 0.5),
        Duration::from_nanos(6),
        "(2^62 + 3) << 2 wraps to 12"
    );
    // A cap that does not fit in i64 nanoseconds is clamped, not wrapped.
    assert_eq!(
        backoff(0, Duration::MAX, Duration::MAX, 0.5),
        Duration::from_nanos((i64::MAX as f64 * 0.5) as u64)
    );
}

#[test]
fn policy_backoff_uses_its_settings() {
    let p = Policy::new(&retry_cfg());
    assert_eq!(p.backoff(0, 0.5), Duration::from_micros(12_500));
    assert_eq!(p.backoff(9, 0.5), Duration::from_millis(125));
}

// ---- the retry decisions from transport.go ----

#[test]
fn idempotent_methods_and_keys() {
    let none = HeaderMap::new();
    for m in [
        Method::GET,
        Method::HEAD,
        Method::OPTIONS,
        Method::TRACE,
        Method::PUT,
        Method::DELETE,
    ] {
        assert!(idempotent(&m, &none), "{m}");
    }
    for m in [Method::POST, Method::PATCH, Method::CONNECT] {
        assert!(!idempotent(&m, &none), "{m}");
    }
    let custom = Method::from_bytes(b"PURGE").unwrap();
    assert!(!idempotent(&custom, &none));
    let mut keyed = HeaderMap::new();
    keyed.insert("Idempotency-Key", HeaderValue::from_static("8e03978e"));
    assert!(idempotent(&Method::POST, &keyed));
    // Go's Header.Get returns "" for an empty value, which does not count.
    let mut empty = HeaderMap::new();
    empty.insert("idempotency-key", HeaderValue::from_static(""));
    assert!(!idempotent(&Method::POST, &empty));
}

#[test]
fn outcomes_classify_like_steer() {
    assert_eq!(Outcome::of_status(200), Outcome::Success);
    assert_eq!(Outcome::of_status(499), Outcome::Success);
    assert_eq!(Outcome::of_status(500), Outcome::Http5xx(500));
    assert_eq!(Outcome::of_status(599), Outcome::Http5xx(599));
    assert_eq!(Outcome::of_status(600), Outcome::Success);
    assert_eq!(
        Outcome::of_error(ClientError::Connect),
        Outcome::ConnectError
    );
    assert_eq!(
        Outcome::of_error(ClientError::ConnectTimeout),
        Outcome::ConnectError
    );
    assert_eq!(Outcome::of_error(ClientError::Timeout), Outcome::Timeout);
    assert_eq!(Outcome::of_error(ClientError::Protocol), Outcome::Reset);
    assert_eq!(
        Outcome::of_error(ClientError::HeadersTooLarge),
        Outcome::Reset
    );

    let all = [
        Outcome::Success,
        Outcome::Http5xx(503),
        Outcome::ConnectError,
        Outcome::Timeout,
        Outcome::Reset,
    ];
    for (i, o) in all.iter().enumerate() {
        assert_eq!(o.index(), i);
        assert_eq!(o.to_string(), Outcome::NAMES[i]);
        assert_eq!(o.is_failure(), i != 0);
    }
    assert_eq!(
        Outcome::NAMES,
        ["success", "http_5xx", "connect_error", "timeout", "reset"]
    );
}

#[test]
fn gateway_failures_are_no_response_or_502_to_504() {
    assert!(!Outcome::Success.is_gateway_failure());
    for s in [502, 503, 504] {
        assert!(Outcome::Http5xx(s).is_gateway_failure(), "{s}");
    }
    for s in [500, 501, 505, 599] {
        assert!(!Outcome::Http5xx(s).is_gateway_failure(), "{s}");
    }
    for o in [Outcome::ConnectError, Outcome::Timeout, Outcome::Reset] {
        assert!(o.is_gateway_failure(), "{o}");
    }
}

#[test]
fn retriable_statuses() {
    let p = Policy::new(&retry_cfg());
    for s in 500..=599 {
        assert_eq!(p.retries_on(s), matches!(s, 502..=504), "{s}");
    }
    assert!(!p.retries_on(499));
    assert!(!p.retries_on(600));
    let mut cfg = retry_cfg();
    cfg.retry_on = vec![500, 599, 200, 700];
    let p = Policy::new(&cfg);
    let on: Vec<u16> = (0..=1000).filter(|&s| p.retries_on(s)).collect();
    assert_eq!(on, [500, 599], "statuses outside 5xx are ignored");
    cfg.retry_on.clear();
    assert!((500..=599).all(|s| !Policy::new(&cfg).retries_on(s)));
}

#[test]
fn which_outcomes_are_retried() {
    let p = Policy::new(&retry_cfg());
    let safe = true;
    let unsafe_ = false;
    // A connect error never reached the upstream: always retried.
    assert!(p.retryable(0, true, unsafe_, Outcome::ConnectError));
    // A timeout or reset may have been acted on: only when safe.
    for o in [Outcome::Timeout, Outcome::Reset] {
        assert!(p.retryable(0, true, safe, o), "{o}");
        assert!(!p.retryable(0, true, unsafe_, o), "{o}");
    }
    // A 5xx when safe and listed.
    assert!(p.retryable(1, true, safe, Outcome::Http5xx(503)));
    assert!(!p.retryable(1, true, unsafe_, Outcome::Http5xx(503)));
    assert!(!p.retryable(1, true, safe, Outcome::Http5xx(500)));
    // Never a success, never past max_retries, never an unreplayable body.
    assert!(!p.retryable(0, true, safe, Outcome::Success));
    assert!(!p.retryable(2, true, safe, Outcome::ConnectError));
    assert!(!p.retryable(0, false, safe, Outcome::ConnectError));
    // max_retries 0 turns retries off.
    let mut cfg = retry_cfg();
    cfg.max_retries = 0;
    assert!(!Policy::new(&cfg).retryable(0, true, safe, Outcome::ConnectError));
}

#[test]
fn per_try_timeout_and_body_buffering() {
    let mut cfg = retry_cfg();
    let p = Policy::new(&cfg);
    assert_eq!(p.per_try_timeout(), None, "zero means no per-try timeout");
    assert_eq!(p.max_retries(), 2);
    assert_eq!(p.max_body_bytes(), 64 << 10);
    assert_eq!(p.buffer_limit(None), Some(64 << 10), "unknown length: try");
    assert_eq!(p.buffer_limit(Some(64 << 10)), Some(64 << 10));
    assert_eq!(p.buffer_limit(Some((64 << 10) + 1)), None);
    cfg.per_try_timeout = Duration::from_millis(300);
    cfg.max_retries = 0;
    let p = Policy::new(&cfg);
    assert_eq!(p.per_try_timeout(), Some(Duration::from_millis(300)));
    assert_eq!(
        p.buffer_limit(Some(0)),
        None,
        "no retries, nothing to buffer"
    );
}

/// Polls `fut` on the current task until the clock has a timer waiting.
async fn until_waiting(clock: &ManualClock, n: usize) {
    while clock.waiters() < n {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn per_try_cuts_off_a_slow_attempt() {
    let clock = Arc::new(ManualClock::new());
    let c = clock.clone();
    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = DropCount(dropped.clone());
    let task = tokio::spawn(async move {
        per_try(&*c, Some(Duration::from_millis(300)), async move {
            let _guard = guard;
            std::future::pending::<()>().await
        })
        .await
    });
    until_waiting(&clock, 1).await;
    assert_eq!(clock.advance(Duration::from_millis(299)), 0);
    assert_eq!(clock.advance(Duration::from_millis(1)), 1);
    assert_eq!(task.await.unwrap(), Err(PerTryTimeout));
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "the attempt was cancelled"
    );
    assert_eq!(PerTryTimeout.to_string(), "per-try timeout");
}

struct DropCount(Arc<AtomicUsize>);

impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn per_try_keeps_an_attempt_that_finishes_in_time() {
    let clock = Arc::new(ManualClock::new());
    let (tx, rx) = tokio::sync::oneshot::channel::<u16>();
    let c = clock.clone();
    let task = tokio::spawn(async move {
        per_try(&*c, Some(Duration::from_secs(1)), async move {
            rx.await.unwrap()
        })
        .await
    });
    until_waiting(&clock, 1).await;
    clock.advance(Duration::from_millis(999));
    tx.send(200).unwrap();
    assert_eq!(task.await.unwrap(), Ok(200));
}

#[tokio::test]
async fn per_try_prefers_the_attempt_when_both_are_ready() {
    let clock = ManualClock::new();
    // A zero timeout is ready at once, and so is the attempt.
    assert_eq!(
        per_try(&clock, Some(Duration::ZERO), async { 7 }).await,
        Ok(7)
    );
    assert_eq!(
        per_try(&clock, Some(Duration::ZERO), std::future::pending::<()>()).await,
        Err(PerTryTimeout)
    );
    // No timeout: the attempt runs to the end, with no timer.
    assert_eq!(per_try(&clock, None, async { 8 }).await, Ok(8));
    assert_eq!(clock.waiters(), 0);
}

// ---- concurrency ----

#[test]
fn concurrent_withdrawals_never_overdraw() {
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new());
    // A floor of 5/s over 10 s, plus 0.5 of 100 requests: 100 retries.
    let b = Arc::new(Budget::new(
        &budget_cfg(0.5, 5, Duration::from_secs(10)),
        clock,
    ));
    for _ in 0..100 {
        b.deposit();
    }
    let granted = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let b = Arc::clone(&b);
            let granted = Arc::clone(&granted);
            std::thread::spawn(move || {
                for _ in 0..1000 {
                    if b.try_withdraw() {
                        granted.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(granted.load(Ordering::Relaxed), 100);
    assert_eq!(b.totals(), (100, 100));
    assert_eq!(b.balance(), 0.0);
}

#[test]
fn concurrent_deposits_and_withdrawals_stay_within_the_ratio() {
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new());
    let b = Arc::new(Budget::new(
        &budget_cfg(0.25, 0, Duration::from_secs(10)),
        clock,
    ));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let b = Arc::clone(&b);
            std::thread::spawn(move || {
                for _ in 0..500 {
                    b.deposit();
                    b.try_withdraw();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let (requests, retries) = b.totals();
    assert_eq!(requests, 4000);
    // Each retry was granted while retries < 0.25 × requests.
    assert!(retries <= 1000, "{retries} retries for {requests} requests");
    assert!(
        retries >= 990,
        "a retry is granted whenever the ratio allows"
    );
}

// ---- properties ----

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => Just(Op::Deposit),
        4 => Just(Op::Withdraw),
        1 => (0u64..2500).prop_map(Op::Advance),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The budget against the reference, decision by decision.
    #[test]
    fn budget_agrees_with_the_reference(
        window in 1u64..=10,
        ratio_tenths in 0u32..=10,
        min_per_second in 0u32..3,
        offset_ms in 0u64..1000,
        ops in prop::collection::vec(op(), 0..400),
    ) {
        let ratio = f64::from(ratio_tenths) / 10.0;
        if let Err(e) = check_against_reference(window, ratio, min_per_second, offset_ms, &ops) {
            prop_assert!(false, "{}", e);
        }
    }

    /// For any trace, retries stay within the budget: each one is granted
    /// only while the retries in its window are below the floor plus the
    /// ratio of the requests in that window, counted from an independent
    /// log. No request retries more than `max_retries` times, or retries a
    /// success, an unreplayable body, or an unsafe request after it may
    /// have reached the upstream.
    #[test]
    fn retries_stay_within_budget_for_any_trace(
        window in 1u64..=10,
        ratio_tenths in 0u32..=10,
        min_per_second in 0u32..3,
        max_retries in 0u32..=5,
        retry_on in prop::collection::vec(500u16..=599, 0..4),
        trace in prop::collection::vec(
            (
                0u64..1500,                // ms before the request
                any::<bool>(),             // safe
                any::<bool>(),             // replayable
                prop::collection::vec(attempt_outcome(), 1..=6),
            ),
            0..150,
        ),
    ) {
        let clock = Arc::new(ManualClock::new());
        let origin = clock.now();
        let ratio = f64::from(ratio_tenths) / 10.0;
        let budget = Budget::with_origin(
            &budget_cfg(ratio, min_per_second, Duration::from_secs(window)),
            clock.clone(),
            origin,
        );
        let mut cfg = retry_cfg();
        cfg.max_retries = max_retries;
        cfg.retry_on = retry_on;
        let policy = Policy::new(&cfg);
        let mut log: Vec<Event> = Vec::new();
        let mut total_retries = 0u64;

        for (i, (gap, safe, replayable, outcomes)) in trace.iter().enumerate() {
            clock.advance(Duration::from_millis(*gap));
            budget.deposit();
            let sec = (clock.now() - origin).as_secs();
            log.push(Event { sec, retry: false });
            let mut retries = 0u32;
            for (attempt, outcome) in outcomes.iter().enumerate() {
                let attempt = attempt as u32;
                if !policy.retryable(attempt, *replayable, *safe, *outcome) {
                    break;
                }
                let sec = (clock.now() - origin).as_secs();
                let (requests, before) = window_counts(&log, sec, window);
                let allowed = f64::from(min_per_second) * window as f64 + ratio * requests;
                let granted = budget.try_withdraw();
                prop_assert_eq!(granted, before < allowed, "request {} attempt {}", i, attempt);
                if !granted {
                    break;
                }
                log.push(Event { sec, retry: true });
                retries += 1;
                // The wait between attempts passes on the clock.
                clock.advance(policy.backoff(attempt, 0.5));
            }
            prop_assert!(retries <= max_retries);
            prop_assert!(*replayable || retries == 0);
            total_retries += u64::from(retries);
        }
        // And a bound over the whole trace: within any one window the
        // floor pays for at most min × window retries, and each request
        // for at most `ratio` more, so the total cannot pass this.
        let windows = log.last().map_or(0, |e| e.sec / window + 1);
        let requests = trace.len() as f64;
        let bound = windows as f64 * f64::from(min_per_second) * window as f64
            + windows as f64 * (ratio * requests + 1.0);
        prop_assert!(total_retries as f64 <= bound, "{} retries over bound {}", total_retries, bound);
    }

    /// Backoff never exceeds the cap or the doubled base, and is never
    /// negative, whatever the settings.
    #[test]
    fn backoff_stays_under_its_ceiling(
        attempt in 0u32..80,
        base_ms in 1u64..=60_000,
        extra_ms in 0u64..=60_000,
        random in 0.0f64..1.0,
    ) {
        let base = Duration::from_millis(base_ms);
        let cap = Duration::from_millis(base_ms + extra_ms);
        let got = backoff(attempt, base, cap, random);
        prop_assert!(got < cap);
        let doubled = (u128::from(base_ms) * 1_000_000) << attempt.min(100);
        if attempt < 32 && doubled < cap.as_nanos() {
            prop_assert!(got.as_nanos() < doubled, "{:?} for a ceiling of {} ns", got, doubled);
        }
    }
}

fn attempt_outcome() -> impl Strategy<Value = Outcome> {
    prop_oneof![
        Just(Outcome::Success),
        (500u16..=599).prop_map(Outcome::Http5xx),
        Just(Outcome::ConnectError),
        Just(Outcome::Timeout),
        Just(Outcome::Reset),
    ]
}

/// Requests and retries in the log whose second lies in the window ending
/// at `now`.
fn window_counts(log: &[Event], now: u64, window: u64) -> (f64, f64) {
    let mut requests = 0.0;
    let mut retries = 0.0;
    for e in log.iter().filter(|e| e.sec + window > now) {
        if e.retry {
            retries += 1.0;
        } else {
            requests += 1.0;
        }
    }
    (requests, retries)
}

#[test]
fn backoff_spreads_draws_over_the_whole_range() {
    // With uniform draws, full jitter fills [0, ceiling): the mean is near
    // half the ceiling and both ends are reached.
    let rng = SplitMix64::new(42);
    let cap = Duration::from_millis(250);
    let base = Duration::from_millis(25);
    let draws: Vec<Duration> = (0..10_000)
        .map(|_| backoff(2, base, cap, unit(&rng)))
        .collect();
    let mean = draws.iter().sum::<Duration>() / draws.len() as u32;
    assert!(
        mean > Duration::from_millis(48) && mean < Duration::from_millis(52),
        "{mean:?}"
    );
    assert!(draws.iter().any(|d| *d < Duration::from_millis(1)));
    assert!(draws.iter().any(|d| *d > Duration::from_millis(99)));
    assert!(draws.iter().all(|d| *d < Duration::from_millis(100)));
}
