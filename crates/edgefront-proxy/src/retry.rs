//! Retries, ported from steer's `internal/retry` and the retry decisions in
//! its `internal/proxy/transport.go`.
//!
//! Four pieces, each usable on its own, so that the request path and the
//! differential driver make the same decisions:
//!
//! - [`Budget`] caps retries at a fraction of recent requests plus a small
//!   floor, in the style of Finagle's `RetryBudget`. Without it an upstream
//!   outage turns every request into `1 + max_retries` requests, and the
//!   extra load keeps the upstream down. With a ratio of 0.2, retries add at
//!   most 20% load, plus the floor, however much fails.
//! - [`Policy`] holds a cluster's retry settings and decides whether an
//!   attempt may be retried: how many retries, which `5xx` statuses, whether
//!   the body can be replayed and whether the method is safe to repeat.
//! - [`Outcome`] classifies an attempt the way steer labels it in
//!   `steer_upstream_attempts_total`: `success`, `http_5xx`,
//!   `connect_error`, `timeout` or `reset`.
//! - [`backoff`] is the wait before a retry, exponential with full jitter,
//!   and [`per_try`] bounds one attempt by the per-try timeout on a
//!   [`Clock`].
//!
//! The order of the checks follows steer: an attempt that may be retried
//! first needs another endpoint from the balancer, and only then is the
//! budget withdrawn from, so a retry that has nowhere to go costs nothing.

use std::fmt;
use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;
use std::time::{Duration, Instant};

use edgefront_core::clock::Clock;
use edgefront_core::config;
use edgefront_core::http1::ClientError;
use http::{HeaderMap, Method};

/// Counts requests and retries in one-second buckets over a sliding window
/// of whole seconds. A retry is allowed while
///
/// ```text
/// retries in window < min_per_second × window_seconds + ratio × requests in window
/// ```
///
/// The comparison is made in `f64` exactly as steer makes it, so the two
/// agree on every decision. A bucket covers one whole second counted from
/// the budget's origin, and the window holds the current second and the
/// `window_seconds - 1` before it.
///
/// It is safe to share between threads: one mutex covers the buckets, so a
/// burst of concurrent withdrawals never takes more than the budget holds.
pub struct Budget {
    ratio: f64,
    min_per_second: u64,
    clock: Arc<dyn Clock>,
    origin: Instant,
    /// A ring indexed by second modulo its length.
    buckets: Mutex<Box<[Bucket]>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Bucket {
    sec: u64,
    requests: u64,
    retries: u64,
}

impl fmt::Debug for Budget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Budget")
            .field("ratio", &self.ratio)
            .field("min_per_second", &self.min_per_second)
            .field("window_seconds", &self.window_seconds())
            .finish_non_exhaustive()
    }
}

impl Budget {
    /// A budget for `cfg`, with seconds counted from the clock's reading
    /// now. The window is rounded down to whole seconds, and is at least
    /// one second.
    pub fn new(cfg: &config::Budget, clock: Arc<dyn Clock>) -> Budget {
        let origin = clock.now();
        Budget::with_origin(cfg, clock, origin)
    }

    /// A budget whose seconds are counted from `origin`. steer buckets by
    /// Unix second; passing the instant of a whole Unix second here puts
    /// the bucket boundaries in the same places, which is what lets the
    /// differential driver compare the two. A reading before `origin`
    /// counts as its first second.
    pub fn with_origin(cfg: &config::Budget, clock: Arc<dyn Clock>, origin: Instant) -> Budget {
        let n = usize::try_from(cfg.window.as_secs())
            .unwrap_or(usize::MAX)
            .clamp(1, MAX_WINDOW_SECONDS);
        Budget {
            ratio: cfg.ratio,
            min_per_second: cfg.min_per_second.into(),
            clock,
            origin,
            buckets: Mutex::new(vec![Bucket::default(); n].into_boxed_slice()),
        }
    }

    /// The length of the window in seconds.
    pub fn window_seconds(&self) -> usize {
        self.lock().len()
    }

    /// Records one original request.
    pub fn deposit(&self) {
        let mut buckets = self.lock();
        let now = self.second();
        current(&mut buckets, now).requests += 1;
    }

    /// Reports whether a retry may be sent now, and records it if so.
    pub fn try_withdraw(&self) -> bool {
        let mut buckets = self.lock();
        let now = self.second();
        let (requests, retries) = totals(&buckets, now);
        if retries as f64 >= self.allowed(buckets.len(), requests) {
            return false;
        }
        current(&mut buckets, now).retries += 1;
        true
    }

    /// How many more retries the budget would allow now. It is not always a
    /// whole number: with a ratio of 0.2 and 3 requests it is 0.6 above the
    /// floor, and a retry is allowed while it is above zero.
    pub fn balance(&self) -> f64 {
        let buckets = self.lock();
        let now = self.second();
        let (requests, retries) = totals(&buckets, now);
        (self.allowed(buckets.len(), requests) - retries as f64).max(0.0)
    }

    /// The requests and retries counted in the window now.
    pub fn totals(&self) -> (u64, u64) {
        let buckets = self.lock();
        totals(&buckets, self.second())
    }

    /// steer's `float64(minPerSec*len(buckets)) + ratio*float64(requests)`.
    fn allowed(&self, window: usize, requests: u64) -> f64 {
        let floor = self.min_per_second.saturating_mul(window as u64);
        floor as f64 + self.ratio * requests as f64
    }

    /// The current second. Read under the lock, so that the seconds the
    /// buckets see never go backwards: a caller holding an older reading
    /// would otherwise recycle a newer bucket.
    fn second(&self) -> u64 {
        self.clock
            .now()
            .saturating_duration_since(self.origin)
            .as_secs()
    }

    /// The counters stay consistent whatever a panicking holder did, since
    /// every update is a single increment or a whole-bucket reset.
    fn lock(&self) -> std::sync::MutexGuard<'_, Box<[Bucket]>> {
        self.buckets.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The longest window a budget keeps, an hour. Config validation allows a
/// minute; this only bounds the memory of a budget built by hand.
pub const MAX_WINDOW_SECONDS: usize = 3600;

/// The bucket for second `now`, recycling a stale one.
fn current(buckets: &mut [Bucket], now: u64) -> &mut Bucket {
    let n = buckets.len() as u64;
    let bucket = &mut buckets[(now % n) as usize];
    if bucket.sec != now {
        *bucket = Bucket {
            sec: now,
            ..Bucket::default()
        };
    }
    bucket
}

/// The sums over the buckets inside the window ending at second `now`.
fn totals(buckets: &[Bucket], now: u64) -> (u64, u64) {
    let n = buckets.len() as u64;
    buckets
        .iter()
        .filter(|b| b.sec <= now && now - b.sec < n)
        .fold((0, 0), |(requests, retries), b| {
            (requests + b.requests, retries + b.retries)
        })
}

/// How one attempt at an upstream ended, as steer classifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// A response with a status below 500.
    Success,
    /// A response with a status from 500 to 599.
    Http5xx(u16),
    /// The connection could not be made, so the request never reached the
    /// upstream.
    ConnectError,
    /// No response head in time: the per-try timeout or the client's
    /// response header timeout.
    Timeout,
    /// The connection broke, or the upstream did not speak HTTP/1.1.
    Reset,
}

impl Outcome {
    /// Every label, in steer's order.
    pub const NAMES: [&'static str; 5] =
        ["success", "http_5xx", "connect_error", "timeout", "reset"];

    /// The outcome of an attempt that got a response head with `status`.
    pub fn of_status(status: u16) -> Outcome {
        if (500..=599).contains(&status) {
            Outcome::Http5xx(status)
        } else {
            Outcome::Success
        }
    }

    /// The outcome of an attempt the upstream client gave up on. A connect
    /// timeout is a connect error, as a dial timeout is in steer.
    pub fn of_error(e: ClientError) -> Outcome {
        match e {
            ClientError::Connect | ClientError::ConnectTimeout => Outcome::ConnectError,
            ClientError::Timeout => Outcome::Timeout,
            ClientError::HeadersTooLarge | ClientError::Protocol => Outcome::Reset,
        }
    }

    /// steer's `result` label.
    pub fn as_str(self) -> &'static str {
        Outcome::NAMES[self.index()]
    }

    /// The position of the label in [`Outcome::NAMES`].
    pub fn index(self) -> usize {
        match self {
            Outcome::Success => 0,
            Outcome::Http5xx(_) => 1,
            Outcome::ConnectError => 2,
            Outcome::Timeout => 3,
            Outcome::Reset => 4,
        }
    }

    /// Whether the attempt failed, for success-rate outlier detection.
    pub fn is_failure(self) -> bool {
        self != Outcome::Success
    }

    /// Whether the attempt counts as a gateway failure for consecutive
    /// failure ejection: no response at all, or a 502, 503 or 504.
    pub fn is_gateway_failure(self) -> bool {
        match self {
            Outcome::Success => false,
            Outcome::Http5xx(status) => matches!(status, 502..=504),
            Outcome::ConnectError | Outcome::Timeout | Outcome::Reset => true,
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a request may be sent twice: the methods RFC 9110 calls
/// idempotent, or any request with an `Idempotency-Key` header that is not
/// empty.
pub fn idempotent(method: &Method, headers: &HeaderMap) -> bool {
    matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE | Method::PUT | Method::DELETE
    ) || headers
        .get("idempotency-key")
        .is_some_and(|v| !v.as_bytes().is_empty())
}

/// A cluster's retry settings, ready for the request path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    max_retries: u32,
    /// Bit `s - 500` is set when status `s` is retried.
    retry_on: u128,
    per_try_timeout: Option<Duration>,
    backoff_base: Duration,
    backoff_max: Duration,
    max_body_bytes: u64,
}

impl Policy {
    /// The policy for `cfg`. Statuses outside 500 to 599, which validation
    /// refuses, are ignored.
    pub fn new(cfg: &config::Retry) -> Policy {
        let retry_on = cfg
            .retry_on
            .iter()
            .filter(|s| (500..=599).contains(*s))
            .fold(0u128, |set, &s| set | 1 << (s - 500));
        Policy {
            max_retries: cfg.max_retries,
            retry_on,
            per_try_timeout: Some(cfg.per_try_timeout).filter(|d| !d.is_zero()),
            backoff_base: cfg.backoff_base,
            backoff_max: cfg.backoff_max,
            max_body_bytes: cfg.max_body_bytes,
        }
    }

    /// The most retries one request may make.
    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }

    /// Whether a `5xx` response with `status` is retried.
    pub fn retries_on(&self, status: u16) -> bool {
        (500..=599).contains(&status) && self.retry_on & (1 << (status - 500)) != 0
    }

    /// The time one attempt has to produce a response head, if limited.
    pub fn per_try_timeout(&self) -> Option<Duration> {
        self.per_try_timeout
    }

    /// The most body bytes buffered so that a request can be replayed.
    pub fn max_body_bytes(&self) -> u64 {
        self.max_body_bytes
    }

    /// How much of a request body to buffer for replay, given its declared
    /// length (`None` when it has none, as with chunked encoding): `None`
    /// when the body should stream to the first endpoint unbuffered.
    ///
    /// As in steer, the caller reads up to the limit plus one byte. A body
    /// that fits is replayable; a longer one streams on, the bytes already
    /// read first, and cannot be retried.
    pub fn buffer_limit(&self, content_length: Option<u64>) -> Option<u64> {
        let fits = content_length.is_none_or(|n| n <= self.max_body_bytes);
        (self.max_retries > 0 && fits).then_some(self.max_body_bytes)
    }

    /// Whether attempt number `attempt` (0 for the first try), which ended
    /// with `outcome`, may be retried. `replayable` says whether its body
    /// can be sent again and `safe` whether the request is
    /// [`idempotent`].
    ///
    /// A connect error is always retried, since the request never reached
    /// the upstream. A timeout or a reset is retried only for a safe
    /// request, and so is a `5xx` whose status is in `retry_on`. A retry
    /// still needs another endpoint and room in the [`Budget`].
    pub fn retryable(&self, attempt: u32, replayable: bool, safe: bool, outcome: Outcome) -> bool {
        if attempt >= self.max_retries || !replayable {
            return false;
        }
        match outcome {
            Outcome::Success => false,
            Outcome::ConnectError => true,
            Outcome::Timeout | Outcome::Reset => safe,
            Outcome::Http5xx(status) => safe && self.retries_on(status),
        }
    }

    /// The wait before retry number `attempt` with this policy's base and
    /// cap; see [`backoff`].
    pub fn backoff(&self, attempt: u32, random: f64) -> Duration {
        backoff(attempt, self.backoff_base, self.backoff_max, random)
    }
}

/// The wait before retry number `attempt` (0 for the first retry):
/// exponential with full jitter, `random × min(max, base × 2^attempt)`,
/// where `random` is a uniform draw from `[0, 1)`. Full jitter spreads
/// synchronised clients out best (Brooker, "Exponential Backoff And
/// Jitter").
///
/// This is steer's `Backoff` to the nanosecond, including how it treats a
/// doubling that overflows: from attempt 32 on, or once `base × 2^attempt`
/// wraps past `i64` nanoseconds, the cap applies.
pub fn backoff(attempt: u32, base: Duration, max: Duration, random: f64) -> Duration {
    let base = nanos(base);
    let max = nanos(max);
    let mut ceiling = max;
    if attempt < 32 {
        let doubled = base.wrapping_shl(attempt);
        if doubled > 0 && doubled < max {
            ceiling = doubled;
        }
    }
    // Go's time.Duration(float64) conversion truncates towards zero.
    let wait = (random * ceiling as f64) as i64;
    Duration::from_nanos(u64::try_from(wait).unwrap_or(0))
}

fn nanos(d: Duration) -> i64 {
    i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
}

/// The error of an attempt cut off by its per-try timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerTryTimeout;

impl fmt::Display for PerTryTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("per-try timeout")
    }
}

impl std::error::Error for PerTryTimeout {}

/// Runs one attempt, `attempt`, for at most `timeout` on `clock`; `None`
/// means no limit. An attempt that finishes in the same poll as its timer
/// fires keeps its result, as the attempt is polled first. The attempt is
/// dropped, cancelling it, when the timeout wins.
pub async fn per_try<F: Future>(
    clock: &dyn Clock,
    timeout: Option<Duration>,
    attempt: F,
) -> Result<F::Output, PerTryTimeout> {
    let mut attempt = pin!(attempt);
    let Some(timeout) = timeout else {
        return Ok(attempt.await);
    };
    let mut timer = pin!(clock.after(timeout));
    std::future::poll_fn(|cx| {
        if let Poll::Ready(out) = attempt.as_mut().poll(cx) {
            return Poll::Ready(Ok(out));
        }
        timer.as_mut().poll(cx).map(|_| Err(PerTryTimeout))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use edgefront_core::clock::ManualClock;

    fn budget(ratio: f64, min: u32, window: u64) -> (Budget, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new());
        let cfg = config::Budget {
            ratio,
            min_per_second: min,
            window: Duration::from_secs(window),
        };
        (Budget::new(&cfg, clock.clone()), clock)
    }

    #[test]
    fn a_bucket_is_recycled_when_its_second_comes_round_again() {
        let (b, clock) = budget(1.0, 0, 2);
        b.deposit();
        assert_eq!(b.totals(), (1, 0));
        clock.advance(Duration::from_secs(2));
        // Same slot, a new second: the old count is gone.
        b.deposit();
        assert_eq!(b.totals(), (1, 0));
        let ring = b.lock();
        assert_eq!(ring[0].sec, 2);
        assert_eq!(ring[0].requests, 1);
    }

    #[test]
    fn a_reading_before_the_origin_is_the_first_second() {
        let clock = Arc::new(ManualClock::new());
        let cfg = config::Budget {
            ratio: 0.0,
            min_per_second: 1,
            window: Duration::from_secs(1),
        };
        let later = clock.now() + Duration::from_secs(5);
        let b = Budget::with_origin(&cfg, clock.clone(), later);
        assert_eq!(b.second(), 0);
        assert!(b.try_withdraw());
        assert!(!b.try_withdraw());
    }

    #[test]
    fn a_huge_window_is_capped() {
        let (b, _) = budget(0.0, 0, u64::MAX);
        assert_eq!(b.window_seconds(), MAX_WINDOW_SECONDS);
        let (b, _) = budget(0.0, u32::MAX, 3600);
        // The floor saturates rather than wrapping.
        assert!(b.balance() > 1e12);
    }

    #[test]
    fn a_poisoned_budget_keeps_counting() {
        let (b, _) = budget(1.0, 0, 1);
        let b = Arc::new(b);
        let held = Arc::clone(&b);
        let _ = std::thread::spawn(move || {
            let _guard = held.lock();
            panic!("poison the budget");
        })
        .join();
        b.deposit();
        assert!(b.try_withdraw());
        assert!(!b.try_withdraw());
    }

    #[test]
    fn debug_shows_the_settings() {
        let (b, _) = budget(0.2, 10, 10);
        let s = format!("{b:?}");
        assert!(s.contains("ratio: 0.2"), "{s}");
        assert!(s.contains("window_seconds: 10"), "{s}");
    }
}
