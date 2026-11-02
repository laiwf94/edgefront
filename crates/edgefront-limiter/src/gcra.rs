//! Generic cell rate algorithm, ported from libpacer's `gcra.c` and
//! `gcra_math.h`.
//!
//! The whole decision state is one 64-bit theoretical arrival time (TAT)
//! updated with compare-and-swap, so takes from any number of threads are
//! lock-free. The rate (interval and tolerance) lives in two more atomic
//! words so that [`Gcra::reconfigure`] can change it while other threads
//! keep taking.
//!
//! Time is kept in ticks of 1/64 ns so that high byte rates keep their
//! precision. The emission interval is rounded up, so the limiter never
//! grants faster than configured; the relative error is below 1/interval,
//! under 0.2% for rates up to 10^8 per second. Faster rates are capped at one
//! token per tick. Timestamps at or past [`MAX_NOW_NS`] (about 4.5 years of
//! uptime) are clamped.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Bits of sub-nanosecond precision: one tick is 1/64 ns.
pub const TICK_SHIFT: u32 = 6;
/// Timestamps at or past this are clamped to one nanosecond before it.
pub const MAX_NOW_NS: u64 = 1 << 57;
/// The largest tolerance (burst times interval) a rate may have, in ticks.
pub const MAX_TOL_TICKS: u64 = 1 << 62;

/// A rate: `tokens` per `period_ns`, with a bucket `burst` tokens deep. A
/// fresh limiter grants `burst` tokens at once, then refills at the rate.
/// Ten per second with a burst of twenty is `Rate::new(10, 1_000_000_000, 20)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rate {
    pub tokens: u64,
    pub period_ns: u64,
    pub burst: u64,
}

impl Rate {
    pub const fn new(tokens: u64, period_ns: u64, burst: u64) -> Self {
        Self {
            tokens,
            period_ns,
            burst,
        }
    }

    /// Validate the rate and compute its interval and tolerance in ticks.
    ///
    /// The interval is `ceil(period * 64 / tokens)`, computed in 128 bits so
    /// nothing wraps. libpacer multiplied the interval by the burst in
    /// wrapping 128-bit arithmetic, so a product past 2^128 could wrap to a
    /// small tolerance and an interval truncated to zero; here any overflow
    /// is rejected as an invalid rate.
    pub fn ticks(&self) -> Result<(u64, u64), Error> {
        if self.tokens == 0 || self.period_ns == 0 || self.burst == 0 {
            return Err(Error::InvalidRate);
        }
        let num = u128::from(self.period_ns) << TICK_SHIFT;
        let iv = num.div_ceil(u128::from(self.tokens));
        let tol = iv
            .checked_mul(u128::from(self.burst))
            .filter(|&tol| tol <= u128::from(MAX_TOL_TICKS))
            .ok_or(Error::InvalidRate)?;
        // tol <= 2^62 and burst >= 1, so both fit in 64 bits.
        Ok((iv as u64, tol as u64))
    }
}

/// Why a call could not be served at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Error {
    /// A rate with zero tokens, period or burst, or a burst so deep that its
    /// tolerance exceeds [`MAX_TOL_TICKS`].
    InvalidRate,
    /// A cost larger than the burst: it can never be granted.
    CostExceedsBurst,
    /// A keyed-table configuration out of range: no entries, too many, or a
    /// shard count that is not a power of two or exceeds the entries.
    InvalidConfig,
    /// A table key that is empty or longer than [`crate::table::KEY_MAX`].
    InvalidKey,
    /// The key is new and its shard of the table has no room for it.
    TableFull,
    /// The key is not in the table, or has no hold to release.
    NoSuchKey,
    /// The key already has as many holds as allowed.
    HoldLimit,
    /// A sliding-window configuration out of range: no slots, more than
    /// [`crate::window::SLOTS_MAX`], or a window shorter than its slot count
    /// in nanoseconds.
    InvalidWindow,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::InvalidRate => "invalid argument",
            Error::CostExceedsBurst => "cost exceeds burst",
            Error::InvalidConfig => "invalid table configuration",
            Error::InvalidKey => "invalid key",
            Error::TableFull => "table full",
            Error::NoSuchKey => "no such key",
            Error::HoldLimit => "concurrency limit reached",
            Error::InvalidWindow => "invalid window configuration",
        })
    }
}

impl std::error::Error for Error {}

/// The outcome of a take.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Decision {
    /// Whether the request was admitted. A take of zero tokens can be
    /// admitted with nothing granted.
    pub allowed: bool,
    /// Tokens granted; 0 when denied.
    pub granted: u64,
    /// Tokens still available after this call.
    pub remaining: u64,
    /// When nothing was granted: how long to wait, in nanoseconds.
    pub retry_after_ns: u64,
}

/// The decision steps as pure functions of the arrival time, shared by the
/// lock-free limiter (inside a CAS loop) and the keyed table (under its
/// shard lock). All times are in ticks.
pub(crate) mod step {
    use super::{Decision, MAX_NOW_NS, TICK_SHIFT};

    /// Nanoseconds to ticks, clamping past the horizon.
    #[inline]
    pub(crate) fn ticks(now_ns: u64) -> u64 {
        now_ns.min(MAX_NOW_NS - 1) << TICK_SHIFT
    }

    /// Ticks to nanoseconds, rounding up.
    #[inline]
    pub(crate) fn ns_ceil(ticks: u64) -> u64 {
        (ticks >> TICK_SHIFT) + u64::from(ticks & ((1 << TICK_SHIFT) - 1) != 0)
    }

    /// Whole tokens available when the bucket is `debt` ticks behind.
    #[inline]
    pub(crate) fn avail(debt: u64, tol: u64, iv: u64) -> u64 {
        if debt >= tol { 0 } else { (tol - debt) / iv }
    }

    /// All-or-nothing take. The caller has checked `cost <= tol / iv`, so
    /// `cost * iv` cannot overflow. Returns the new arrival time when
    /// granted.
    #[inline]
    pub(crate) fn take(
        tat: u64,
        now: u64,
        cost: u64,
        iv: u64,
        tol: u64,
        d: &mut Decision,
    ) -> Option<u64> {
        let base = tat.max(now);
        let want = base + cost * iv;
        if want - now > tol {
            *d = Decision {
                allowed: false,
                granted: 0,
                remaining: avail(base - now, tol, iv),
                retry_after_ns: ns_ceil(want - now - tol),
            };
            return None;
        }
        *d = Decision {
            allowed: true,
            granted: cost,
            remaining: avail(want - now, tol, iv),
            retry_after_ns: 0,
        };
        Some(want)
    }

    /// Take between `min` and `max` tokens: as many as are available up to
    /// `max`, or nothing if fewer than `min` are. Returns the grant and the
    /// new arrival time when the grant is non-zero. A `min` above the burst
    /// is lowered to the burst, and a `min` of 0 counts as 1.
    #[inline]
    pub(crate) fn range(
        tat: u64,
        now: u64,
        min: u64,
        max: u64,
        iv: u64,
        tol: u64,
        d: &mut Decision,
    ) -> Option<(u64, u64)> {
        let base = tat.max(now);
        let available = avail(base - now, tol, iv);
        let min = min.clamp(1, (tol / iv).max(1));
        *d = Decision {
            allowed: false,
            granted: 0,
            remaining: available,
            retry_after_ns: 0,
        };
        if max == 0 {
            return None;
        }
        if available < min {
            // min tokens fit once base - now drops to tol - min * iv.
            d.retry_after_ns = ns_ceil(base - now + min * iv - tol);
            return None;
        }
        let grant = available.min(max);
        d.allowed = true;
        d.granted = grant;
        d.remaining = available - grant;
        Some((grant, base + grant * iv))
    }

    /// Arrival time after giving back `tokens`. Never moves before `now`,
    /// so a refund cannot create more than a full bucket. The debt can
    /// exceed the tolerance after a reconfigure shrinks the burst, so a
    /// refund is measured against the debt, not capped at one burst. When
    /// `tokens <= debt / iv`, `tokens * iv <= debt` and cannot overflow.
    #[inline]
    pub(crate) fn refund(tat: u64, now: u64, tokens: u64, iv: u64) -> u64 {
        if tat <= now {
            return tat;
        }
        let debt = tat - now;
        if tokens > debt / iv {
            now
        } else {
            tat - tokens * iv
        }
    }
}

/// A lock-free GCRA limiter. It may be embedded anywhere and shared by
/// reference between threads; there is nothing to tear down.
///
/// The state publishes nothing besides itself, so relaxed ordering is
/// enough everywhere: the CAS alone makes each update atomic, and a failed
/// CAS hands back the current value for the next attempt.
#[derive(Debug)]
pub struct Gcra {
    tat: AtomicU64,
    interval: AtomicU64,
    tolerance: AtomicU64,
}

impl Gcra {
    /// A limiter with a full bucket.
    pub fn new(rate: &Rate) -> Result<Self, Error> {
        let (iv, tol) = rate.ticks()?;
        Ok(Self {
            tat: AtomicU64::new(0),
            interval: AtomicU64::new(iv),
            tolerance: AtomicU64::new(tol),
        })
    }

    #[inline]
    fn config(&self) -> (u64, u64) {
        (
            self.interval.load(Ordering::Relaxed),
            self.tolerance.load(Ordering::Relaxed),
        )
    }

    /// Take `cost` tokens at `now_ns`, all or nothing. A denied take leaves
    /// the state unchanged and its `retry_after_ns` says when the same cost
    /// would fit. A cost of 0 takes nothing and only reports `remaining`;
    /// it is allowed unless a reconfigure has left more debt than the new
    /// burst.
    pub fn take(&self, now_ns: u64, cost: u64) -> Result<Decision, Error> {
        let (iv, tol) = self.config();
        let now = step::ticks(now_ns);
        if cost > tol / iv {
            return Err(Error::CostExceedsBurst);
        }
        let mut d = Decision::default();
        let mut tat = self.tat.load(Ordering::Relaxed);
        while let Some(next) = step::take(tat, now, cost, iv, tol, &mut d) {
            match self
                .tat
                .compare_exchange_weak(tat, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return Ok(d),
                Err(current) => tat = current,
            }
        }
        Ok(d)
    }

    /// Tokens available at `now_ns` without taking any.
    pub fn remaining(&self, now_ns: u64) -> u64 {
        let (iv, tol) = self.config();
        let now = step::ticks(now_ns);
        let tat = self.tat.load(Ordering::Relaxed);
        step::avail(tat.saturating_sub(now), tol, iv)
    }

    /// Take as many tokens as are available, up to `max`. When nothing is
    /// granted, `retry_after_ns` says when one token will be ready. Used for
    /// byte shaping, where a read is sized by the grant and the unused part
    /// handed back with [`Gcra::refund`].
    pub fn take_upto(&self, now_ns: u64, max: u64) -> Decision {
        self.take_range(now_ns, 1, max)
    }

    /// Like [`Gcra::take_upto`], but grant nothing unless at least `min`
    /// tokens are available; then `retry_after_ns` says when `min` will be.
    /// Several consumers sharing one limiter use a `min` so that none of
    /// them can keep the others out by taking every token the moment it
    /// appears. A `min` above the burst is lowered to the burst.
    pub fn take_range(&self, now_ns: u64, min: u64, max: u64) -> Decision {
        let (iv, tol) = self.config();
        let now = step::ticks(now_ns);
        let mut d = Decision::default();
        let mut tat = self.tat.load(Ordering::Relaxed);
        while let Some((_, next)) = step::range(tat, now, min, max, iv, tol, &mut d) {
            match self
                .tat
                .compare_exchange_weak(tat, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return d,
                Err(current) => tat = current,
            }
        }
        d
    }

    /// Give back `tokens` previously granted and not used. The state never
    /// moves before `now_ns`, so a refund cannot create more than a full
    /// bucket.
    pub fn refund(&self, now_ns: u64, tokens: u64) {
        let iv = self.interval.load(Ordering::Relaxed);
        let now = step::ticks(now_ns);
        let mut tat = self.tat.load(Ordering::Relaxed);
        loop {
            let next = step::refund(tat, now, tokens, iv);
            if next == tat {
                return;
            }
            match self
                .tat
                .compare_exchange_weak(tat, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(current) => tat = current,
            }
        }
    }

    /// Change the rate while other threads keep taking. Debt is kept as
    /// time: a client that had to wait 2 s still waits 2 s. A take running
    /// at the same moment may combine the old interval with the new
    /// tolerance; the result is bounded by the larger of the two
    /// configurations. An invalid rate leaves the limiter unchanged.
    pub fn reconfigure(&self, rate: &Rate) -> Result<(), Error> {
        let (iv, tol) = rate.ticks()?;
        self.interval.store(iv, Ordering::Relaxed);
        self.tolerance.store(tol, Ordering::Relaxed);
        Ok(())
    }

    /// The theoretical arrival time in ticks, for differential tests and
    /// state handoff.
    pub fn tat(&self) -> u64 {
        self.tat.load(Ordering::Relaxed)
    }

    /// The emission interval in ticks per token.
    pub fn interval(&self) -> u64 {
        self.interval.load(Ordering::Relaxed)
    }

    /// The tolerance (burst times interval) in ticks.
    pub fn tolerance(&self) -> u64 {
        self.tolerance.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    //! Ported one for one from libpacer's tests/test_gcra.c, plus the
    //! concurrency checks the C suite ran in test_concurrency.c.

    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    const SEC: u64 = 1_000_000_000;
    const MS: u64 = 1_000_000;

    fn gcra(tokens: u64, period_ns: u64, burst: u64) -> Gcra {
        Gcra::new(&Rate::new(tokens, period_ns, burst)).unwrap()
    }

    fn allowed(g: &Gcra, now: u64, cost: u64) -> bool {
        g.take(now, cost).unwrap().allowed
    }

    #[test]
    fn rejects_invalid_rates() {
        for r in [
            Rate::new(0, SEC, 1),
            Rate::new(1, 0, 1),
            Rate::new(1, SEC, 0),
            Rate::new(1, SEC, u64::MAX),
        ] {
            assert_eq!(Gcra::new(&r).unwrap_err(), Error::InvalidRate, "{r:?}");
        }
    }

    #[test]
    fn rejects_a_tolerance_that_wraps_128_bits() {
        // interval 2^66 ticks times burst 2^62 is exactly 2^128: libpacer
        // wrapped it to a tolerance of 0 and an interval of 0.
        let r = Rate::new(1, 1 << 60, 1 << 62);
        assert_eq!(r.ticks(), Err(Error::InvalidRate));
        let r = Rate::new(1, u64::MAX, u64::MAX);
        assert_eq!(r.ticks(), Err(Error::InvalidRate));
    }

    #[test]
    fn burst_then_deny() {
        let g = gcra(10, SEC, 5);
        let now = 42 * SEC;
        for i in 0..5 {
            let d = g.take(now, 1).unwrap();
            assert!(d.allowed);
            assert_eq!(d.granted, 1);
            assert_eq!(d.remaining, 4 - i);
        }
        let d = g.take(now, 1).unwrap();
        assert!(!d.allowed);
        assert_eq!(d.granted, 0);
        assert_eq!(d.remaining, 0);
        assert_eq!(d.retry_after_ns, 100 * MS);
    }

    #[test]
    fn retry_after_is_exact() {
        let g = gcra(10, SEC, 5);
        let now = SEC;
        let d = loop {
            let d = g.take(now, 1).unwrap();
            if !d.allowed {
                break d;
            }
        };
        let wait = d.retry_after_ns;
        assert!(!allowed(&g, now + wait - 1, 1));
        assert!(allowed(&g, now + wait, 1));
        // A denied multi-token take reports when that whole cost would fit.
        let d = g.take(now + wait, 3).unwrap();
        assert!(!d.allowed);
        assert!(allowed(&g, now + wait + d.retry_after_ns, 3));
    }

    #[test]
    fn steady_rate_never_denied() {
        let g = gcra(1000, SEC, 1);
        let denied = (0..100_000)
            .filter(|i| !allowed(&g, SEC + i * MS, 1))
            .count();
        assert_eq!(denied, 0);
    }

    #[test]
    fn long_run_rate_matches_config() {
        // Hammer every microsecond for 10 s at 100/s, burst 10: exactly
        // 10 + 100 * 10 tokens are granted.
        let g = gcra(100, SEC, 10);
        let granted = (0..=10 * SEC)
            .step_by(1000)
            .filter(|t| allowed(&g, SEC + t, 1))
            .count();
        assert_eq!(granted, 1010);
    }

    #[test]
    fn cost_larger_than_burst_is_an_error() {
        let g = gcra(10, SEC, 5);
        assert_eq!(g.take(SEC, 6), Err(Error::CostExceedsBurst));
        assert!(allowed(&g, SEC, 5));
    }

    #[test]
    fn zero_cost_reports_without_taking() {
        let g = gcra(10, SEC, 5);
        let d = g.take(SEC, 0).unwrap();
        assert!(d.allowed);
        assert_eq!(d.granted, 0);
        assert_eq!(d.remaining, 5);
        assert_eq!(g.remaining(SEC), 5);
    }

    #[test]
    fn zero_cost_is_denied_while_debt_exceeds_the_burst() {
        let g = gcra(1000, SEC, 100);
        assert_eq!(g.take_upto(SEC, 100).granted, 100);
        g.reconfigure(&Rate::new(1000, SEC, 10)).unwrap();
        // 100 ms of debt against a 10 ms bucket: 90 ms until the level is 0.
        let d = g.take(SEC, 0).unwrap();
        assert!(!d.allowed);
        assert_eq!(d.retry_after_ns, 90 * MS);
        let d = g.take(SEC + 90 * MS, 0).unwrap();
        assert!(d.allowed);
        assert_eq!(d.remaining, 0);
    }

    #[test]
    fn remaining_refills_over_time() {
        let g = gcra(10, SEC, 5);
        assert!(allowed(&g, SEC, 5));
        assert_eq!(g.remaining(SEC), 0);
        assert_eq!(g.remaining(SEC + 250 * MS), 2);
        assert_eq!(g.remaining(SEC + 10 * SEC), 5);
    }

    #[test]
    fn rounds_interval_up_never_over_grants() {
        // 3 per second does not divide a second evenly in ticks. Rounding
        // the interval up means a full second never yields more than
        // 3 + burst.
        let g = gcra(3, SEC, 1);
        let granted = (0..3 * SEC)
            .step_by(MS as usize)
            .filter(|t| allowed(&g, SEC + t, 1))
            .count();
        assert!((9..=10).contains(&granted), "{granted}");
    }

    #[test]
    fn high_byte_rates_keep_precision() {
        // 1 GB/s is exactly 64 ticks per byte.
        assert_eq!(Rate::new(SEC, SEC, 1 << 20).ticks(), Ok((64, 64 << 20)));
        let (iv, _) = Rate::new(u64::MAX, 1, 1).ticks().unwrap();
        assert_eq!(iv, 1);
    }

    #[test]
    fn clamps_time_past_the_horizon() {
        let g = gcra(1, SEC, 1);
        assert!(allowed(&g, u64::MAX, 1));
        assert!(!allowed(&g, u64::MAX, 1));
        assert_eq!(step::ticks(MAX_NOW_NS), step::ticks(MAX_NOW_NS - 1));
    }

    #[test]
    fn errors_describe_themselves() {
        assert_eq!(Error::InvalidRate.to_string(), "invalid argument");
        assert_eq!(Error::CostExceedsBurst.to_string(), "cost exceeds burst");
    }

    #[test]
    fn take_upto_grants_what_is_available() {
        let g = gcra(1000, SEC, 100);
        let d = g.take_upto(SEC, 60);
        assert_eq!((d.granted, d.remaining), (60, 40));
        let d = g.take_upto(SEC, 60);
        assert_eq!((d.granted, d.remaining), (40, 0));
        let d = g.take_upto(SEC, 60);
        assert!(!d.allowed);
        assert_eq!(d.granted, 0);
        assert_eq!(d.retry_after_ns, MS);
        assert_eq!(g.take_upto(SEC + MS, 60).granted, 1);
        assert_eq!(g.take_upto(SEC + 11 * MS, 60).granted, 10);
    }

    #[test]
    fn take_upto_zero_asks_for_nothing() {
        let g = gcra(10, SEC, 1);
        let d = g.take_upto(SEC, 0);
        assert_eq!(d.granted, 0);
        assert_eq!(d.retry_after_ns, 0);
        assert_eq!(d.remaining, 1);
    }

    #[test]
    fn refund_restores_state() {
        let g = gcra(1000, SEC, 100);
        assert_eq!(g.take_upto(SEC, 100).granted, 100);
        g.refund(SEC, 30);
        assert_eq!(g.remaining(SEC), 30);
        g.refund(SEC, 70);
        assert_eq!(g.remaining(SEC), 100);
    }

    #[test]
    fn refund_never_exceeds_a_full_bucket() {
        let g = gcra(1000, SEC, 100);
        assert_eq!(g.take_upto(SEC, 10).granted, 10);
        g.refund(SEC, 500);
        assert_eq!(g.remaining(SEC), 100);
        g.refund(SEC, u64::MAX);
        assert_eq!(g.remaining(SEC), 100);
        // Refunding into an idle limiter changes nothing.
        g.refund(5 * SEC, 10);
        assert_eq!(g.take_upto(5 * SEC, 1000).granted, 100);
    }

    #[test]
    fn reconfigure_keeps_debt_in_time() {
        let g = gcra(1, SEC, 1);
        assert!(allowed(&g, SEC, 1));
        g.reconfigure(&Rate::new(1000, SEC, 1)).unwrap();
        // The client owed one second at the old rate; that wait still stands.
        let d = g.take(SEC, 1).unwrap();
        assert!(!d.allowed);
        assert_eq!(d.retry_after_ns, SEC);
        assert!(!allowed(&g, 2 * SEC - 1, 1));
        assert!(allowed(&g, 2 * SEC, 1));
        let before = (g.interval(), g.tolerance());
        assert_eq!(
            g.reconfigure(&Rate::new(0, SEC, 1)),
            Err(Error::InvalidRate)
        );
        assert_eq!((g.interval(), g.tolerance()), before);
    }

    #[test]
    fn refund_after_a_shrink_gives_back_every_token() {
        // 100 tokens of debt at 1 ms each, then the burst shrinks to 10: the
        // debt stays 100 ms, 90 tokens more than the new bucket holds.
        let g = gcra(1000, SEC, 100);
        assert_eq!(g.take_upto(SEC, 100).granted, 100);
        g.reconfigure(&Rate::new(1000, SEC, 10)).unwrap();
        // Giving back 50 leaves 50 ms of debt, so one token is 41 ms away. A
        // refund capped at one burst would leave it 81 ms away.
        g.refund(SEC, 50);
        let d = g.take(SEC, 1).unwrap();
        assert!(!d.allowed);
        assert_eq!(d.retry_after_ns, 41 * MS);
        g.refund(SEC, u64::MAX);
        assert_eq!(g.remaining(SEC), 10);
    }

    #[test]
    fn take_range_waits_for_the_minimum() {
        let g = gcra(1000, SEC, 100);
        assert_eq!(g.take_upto(SEC, 95).granted, 95);
        // 5 left: a minimum of 10 is not met; 10 are ready 5 ms later.
        let d = g.take_range(SEC, 10, 50);
        assert_eq!((d.granted, d.remaining, d.retry_after_ns), (0, 5, 5 * MS));
        assert_eq!(g.take_range(SEC + 5 * MS, 10, 50).granted, 10);
        // A minimum above the burst is lowered to the burst.
        assert_eq!(g.take_range(10 * SEC, 1000, 1000).granted, 100);
        // A minimum of 0 behaves like 1.
        let d = g.take_range(10 * SEC, 0, 5);
        assert_eq!(d.granted, 0);
        assert_eq!(d.retry_after_ns, MS);
    }

    #[test]
    fn tat_is_the_whole_decision_state() {
        let g = gcra(1000, SEC, 10);
        assert_eq!(g.tat(), 0);
        assert!(allowed(&g, SEC, 1));
        // A fresh bucket is based at now: one token moves the arrival time
        // one interval past it.
        assert_eq!(g.tat(), step::ticks(SEC) + g.interval());
        assert_eq!(g.tolerance(), 10 * g.interval());
    }

    #[test]
    fn concurrent_takes_never_over_grant() {
        // Eight threads race for a burst of 10,000 at one instant: exactly
        // the burst is granted, however the CAS retries interleave.
        let g = Arc::new(gcra(1, SEC, 10_000));
        let granted = Arc::new(AtomicU64::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (g, granted) = (Arc::clone(&g), Arc::clone(&granted));
                std::thread::spawn(move || {
                    for _ in 0..5_000 {
                        if g.take(SEC, 1).unwrap().allowed {
                            granted.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(granted.load(Ordering::Relaxed), 10_000);
        assert_eq!(g.remaining(SEC), 0);
    }

    #[test]
    fn concurrent_upto_and_refund_balance_out() {
        // Each thread takes up to 7 and refunds 3 of whatever it got, so
        // the net take is the grant minus the refund; at one instant the
        // nets add up to the burst exactly once the bucket is drained.
        let g = Arc::new(gcra(1, SEC, 4_000));
        let net = Arc::new(AtomicU64::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (g, net) = (Arc::clone(&g), Arc::clone(&net));
                std::thread::spawn(move || {
                    for _ in 0..2_000 {
                        let got = g.take_upto(SEC, 7).granted;
                        let back = got.min(3);
                        g.refund(SEC, back);
                        net.fetch_add(got - back, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let left = g.remaining(SEC);
        assert_eq!(net.load(Ordering::Relaxed) + left, 4_000);
    }

    #[test]
    fn concurrent_reconfigure_stays_within_the_larger_rate() {
        // One thread flips between bursts of 50 and 100 while others take
        // at a fixed instant: no more than the larger burst is ever granted.
        let g = Arc::new(gcra(1, SEC, 50));
        let granted = Arc::new(AtomicU64::new(0));
        let mut threads: Vec<_> = (0..4)
            .map(|_| {
                let (g, granted) = (Arc::clone(&g), Arc::clone(&granted));
                std::thread::spawn(move || {
                    for _ in 0..1_000 {
                        if let Ok(d) = g.take(SEC, 1) {
                            granted.fetch_add(d.granted, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        let flipper = Arc::clone(&g);
        threads.push(std::thread::spawn(move || {
            for i in 0..1_000 {
                let burst = if i % 2 == 0 { 100 } else { 50 };
                flipper.reconfigure(&Rate::new(1, SEC, burst)).unwrap();
            }
        }));
        for t in threads {
            t.join().unwrap();
        }
        assert!(granted.load(Ordering::Relaxed) <= 100);
    }
}
