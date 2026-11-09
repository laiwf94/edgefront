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
//! token per tick.
//!
//! libpacer stored the TAT as absolute ticks, which run out at 2^57 ns
//! (about 4.5 years of uptime); past that it clamped every timestamp and
//! its limiters stopped refilling. [`Gcra`] stores the TAT as an offset from
//! a moving epoch instead (see [`crate::clock`]), so any `u64` timestamp
//! works and decisions equal exact arithmetic on absolute time.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::clock::{self, EPOCH_TICKS};

/// Bits of sub-nanosecond precision: one tick is 1/64 ns.
pub const TICK_SHIFT: u32 = 6;
/// libpacer's horizon, about 4.5 years: its absolute arrival times stopped
/// here, and it clamped timestamps at or past it to one nanosecond before.
/// Neither [`Gcra`] nor [`crate::Table`] has a horizon (see
/// [`crate::clock`]); the differential tests apply the clamp themselves.
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
/// The decision steps as pure functions of the arrival time, written once
/// for `u64` ([`step`]: the lock-free fast path and the keyed table) and for
/// `u128` ([`wide`]: the epoch slow path, on absolute ticks that need up to
/// 70 bits). All times are in ticks.
macro_rules! decision_steps {
    ($t:ty) => {
        type T = $t;

        #[inline]
        fn out(v: T) -> u64 {
            u64::try_from(v).unwrap_or(u64::MAX)
        }

        /// Ticks to nanoseconds, rounding up.
        #[inline]
        pub(crate) fn ns_ceil(ticks: T) -> u64 {
            out((ticks >> TICK_SHIFT) + T::from(ticks & ((1 << TICK_SHIFT) - 1) != 0))
        }

        /// Whole tokens available when the bucket is `debt` ticks behind.
        #[inline]
        pub(crate) fn avail(debt: T, tol: T, iv: T) -> u64 {
            if debt >= tol {
                0
            } else {
                out((tol - debt) / iv)
            }
        }

        /// All-or-nothing take. The caller has checked `cost <= tol / iv`,
        /// so `cost * iv` cannot overflow. Returns the new arrival time when
        /// granted.
        #[inline]
        pub(crate) fn take(
            tat: T,
            now: T,
            cost: u64,
            iv: T,
            tol: T,
            d: &mut Decision,
        ) -> Option<T> {
            let base = tat.max(now);
            let want = base + T::from(cost) * iv;
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

        /// Take between `min` and `max` tokens: as many as are available up
        /// to `max`, or nothing if fewer than `min` are. Returns the grant
        /// and the new arrival time when the grant is non-zero. A `min`
        /// above the burst is lowered to the burst, and a `min` of 0 counts
        /// as 1.
        #[inline]
        pub(crate) fn range(
            tat: T,
            now: T,
            min: u64,
            max: u64,
            iv: T,
            tol: T,
            d: &mut Decision,
        ) -> Option<(u64, T)> {
            let base = tat.max(now);
            let available = avail(base - now, tol, iv);
            let min = min.clamp(1, out(tol / iv).max(1));
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
                d.retry_after_ns = ns_ceil(base - now + T::from(min) * iv - tol);
                return None;
            }
            let grant = available.min(max);
            d.allowed = true;
            d.granted = grant;
            d.remaining = available - grant;
            Some((grant, base + T::from(grant) * iv))
        }

        /// Arrival time after giving back `tokens`. Never moves before
        /// `now`, so a refund cannot create more than a full bucket. The
        /// debt can exceed the tolerance after a reconfigure shrinks the
        /// burst, so a refund is measured against the debt, not capped at
        /// one burst. When `tokens <= debt / iv`, `tokens * iv <= debt` and
        /// cannot overflow.
        #[inline]
        pub(crate) fn refund(tat: T, now: T, tokens: u64, iv: T) -> T {
            if tat <= now {
                return tat;
            }
            let debt = tat - now;
            if T::from(tokens) > debt / iv {
                now
            } else {
                tat - T::from(tokens) * iv
            }
        }
    };
}

#[allow(clippy::useless_conversion)]
pub(crate) mod step {
    use super::{Decision, TICK_SHIFT};

    decision_steps!(u64);
}

#[allow(clippy::useless_conversion)]
pub(crate) mod wide {
    use super::{Decision, TICK_SHIFT};

    decision_steps!(u128);
}

/// The top bit of the TAT word: the parity of the epoch its offset counts
/// from.
const PARITY: u64 = 1 << 63;
/// The largest offset the TAT word holds: just under two epochs.
const OFFSET_MAX: u64 = PARITY - 1;

/// The epoch and offset a TAT word stands for, given the limiter's base
/// epoch. The word counts from the base or the one after it, and its top
/// bit says which.
#[inline]
fn decode(base: u64, word: u64) -> (u64, u64) {
    (base + ((word >> 63) ^ (base & 1)), word & OFFSET_MAX)
}

#[inline]
fn encode(epoch: u64, offset: u64) -> u64 {
    ((epoch & 1) << 63) | offset
}

/// The TAT word for absolute arrival time `tat`, stored from `epoch` or
/// later and never earlier than `base`. `None` when even `base + 1` is too
/// early to hold it: the base has to move first.
fn place(base: u64, epoch: u64, tat: u128) -> Option<u64> {
    let start = clock::epoch_start(epoch);
    if tat < start {
        // Only a refund can move the TAT back, and only below this epoch's
        // start when `now` is before it. Move back to the base epoch if the
        // word counts from the one after it; a time before the base epoch
        // itself is stored as its start (see `Gcra`'s contract).
        if epoch == base {
            return Some(encode(base, 0));
        }
        let off = tat.saturating_sub(clock::epoch_start(base));
        return Some(encode(base, off as u64));
    }
    let over = tat - start;
    let mut e = epoch;
    if over > u128::from(OFFSET_MAX) {
        e += (over - u128::from(OFFSET_MAX)).div_ceil(u128::from(EPOCH_TICKS)) as u64;
    }
    if e > base + 1 {
        return None;
    }
    Some(encode(e, (tat - clock::epoch_start(e)) as u64))
}

/// A lock-free GCRA limiter. It may be embedded anywhere and shared by
/// reference between threads; there is nothing to tear down.
///
/// The arrival time is an offset from an epoch, kept in two words: `base`,
/// the limiter's base epoch, which only grows, and the TAT word, whose top
/// bit is the parity of the epoch its 63-bit offset counts from, which is
/// always `base` or `base + 1`. An offset spans just under two epochs, so a
/// limiter whose clock is in the epoch after the TAT's moves the TAT word
/// to that epoch on its next write, and moves `base` up behind it when an
/// arrival time would not fit otherwise. Each of those is one CAS, so
/// every step stays lock-free; a limiter left idle for a century catches
/// up in about 90 of them, once.
///
/// The slow path loads `base`, the TAT word and `base` again and retries
/// if `base` moved, so it never pairs a word with the wrong epoch; the fast
/// path, for a TAT and a timestamp in the same epoch, loads the base once. Writers never
/// move the word to an epoch before the base, and the base only moves when
/// the word counts from the epoch after it. One contract follows: callers
/// sharing a limiter must use timestamps within one epoch (2^56 ns, about
/// 2.28 years) of each other, which any monotonic clock gives. A timestamp
/// earlier than the start of the limiter's base epoch counts as that start
/// when a refund would move the arrival time before it.
///
/// Apart from that the state publishes nothing besides itself: the CAS on
/// the TAT word makes each update atomic, and release and acquire order it
/// against the base.
#[derive(Debug)]
pub struct Gcra {
    base: AtomicU64,
    tat: AtomicU64,
    interval: AtomicU64,
    tolerance: AtomicU64,
}

/// What a write to the TAT word will be, worked out on absolute ticks.
enum Plan {
    /// Nothing to write.
    Done(Decision),
    /// Swap in this word.
    Write(u64, Decision),
    /// The base epoch has to move first.
    Lift,
}

impl Gcra {
    /// A limiter with a full bucket.
    pub fn new(rate: &Rate) -> Result<Self, Error> {
        let (iv, tol) = rate.ticks()?;
        Ok(Self {
            base: AtomicU64::new(0),
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

    /// The base epoch and a TAT word that counts from it or the next one.
    #[inline]
    fn snapshot(&self) -> (u64, u64) {
        loop {
            let base = self.base.load(Ordering::Acquire);
            let word = self.tat.load(Ordering::Acquire);
            if self.base.load(Ordering::Acquire) == base {
                return (base, word);
            }
        }
    }

    /// Where the fast path starts: the parity tag of `now_ns`'s epoch, its
    /// offset in ticks and the TAT word, or `None` when the base is not
    /// `now_ns`'s epoch or the one before. The word counts from now's epoch
    /// exactly when its parity bit equals the tag. The base cannot move past
    /// now's epoch under a caller within one epoch of this one, so unlike
    /// `snapshot` this needs no second load of it.
    #[inline(always)]
    fn fast_start(&self, now_ns: u64) -> Option<(u64, u64, u64)> {
        let (now_epoch, now_off) = clock::split(now_ns);
        let base = self.base.load(Ordering::Acquire);
        if now_epoch.wrapping_sub(base) > 1 {
            return None;
        }
        Some((
            (now_epoch & 1) << 63,
            now_off,
            self.tat.load(Ordering::Acquire),
        ))
    }

    /// The usual case of a read-modify-write: the TAT word and `now_ns` in
    /// the same epoch, and the new arrival time still fits in it. `step`
    /// works on offsets and returns the new one, if any. `None` when the
    /// case does not apply and [`Gcra::update`] has to take over.
    #[inline(always)]
    fn update_fast<F>(&self, now_ns: u64, step: F) -> Option<Decision>
    where
        F: Fn(u64, u64, &mut Decision) -> Option<u64>,
    {
        let (tag, now_off, mut word) = self.fast_start(now_ns)?;
        let mut d = Decision::default();
        loop {
            if word & PARITY != tag {
                return None;
            }
            let next = match step(word & OFFSET_MAX, now_off, &mut d) {
                None => return Some(d),
                Some(next) if next <= OFFSET_MAX => tag | next,
                Some(_) => return None,
            };
            match self
                .tat
                .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(d),
                Err(current) => word = current,
            }
        }
    }

    /// One read-modify-write of the arrival time at `now_ns`, in any epoch.
    /// `fast` works on offsets when the TAT and `now_ns` share an epoch;
    /// `wide` works on absolute ticks otherwise. Each returns the new
    /// arrival time, if any, and the decision.
    #[cold]
    #[inline(never)]
    fn update<F, W>(&self, now_ns: u64, fast: F, wide: W) -> Decision
    where
        F: Fn(u64, u64) -> (Option<u64>, Decision),
        W: Fn(u128, u128) -> (Option<u128>, Decision),
    {
        let (now_epoch, now_off) = clock::split(now_ns);
        loop {
            let (base, word) = self.snapshot();
            let (epoch, off) = decode(base, word);
            let plan = match (epoch == now_epoch).then(|| fast(off, now_off)) {
                Some((None, d)) => Plan::Done(d),
                Some((Some(next), d)) if next <= OFFSET_MAX => Plan::Write(encode(epoch, next), d),
                _ => {
                    let tat = clock::epoch_start(epoch) + u128::from(off);
                    match wide(tat, clock::ticks_wide(now_ns)) {
                        (None, d) => Plan::Done(d),
                        (Some(next), d) => match place(base, epoch, next) {
                            Some(w) => Plan::Write(w, d),
                            None => Plan::Lift,
                        },
                    }
                }
            };
            match plan {
                Plan::Done(d) => return d,
                Plan::Write(next, d) => {
                    if next == word
                        || self
                            .tat
                            .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Relaxed)
                            .is_ok()
                    {
                        return d;
                    }
                }
                Plan::Lift => self.lift(base, word, epoch, off),
            }
        }
    }

    /// Move the base epoch up by one: first the TAT word into the epoch
    /// after the base, if it is not there yet, then the base behind it.
    /// Either CAS may lose to another thread, which then made the same
    /// progress. Called only when the new arrival time needs an epoch past
    /// `base + 1`, so `now` is past `base + 2` and an offset that drops
    /// below zero here was already in the past.
    #[cold]
    fn lift(&self, base: u64, word: u64, epoch: u64, off: u64) {
        if epoch == base {
            let moved = encode(base + 1, off.saturating_sub(EPOCH_TICKS));
            let _ = self
                .tat
                .compare_exchange(word, moved, Ordering::AcqRel, Ordering::Relaxed);
        } else {
            let _ = self
                .base
                .compare_exchange(base, base + 1, Ordering::AcqRel, Ordering::Relaxed);
        }
    }

    /// Take `cost` tokens at `now_ns`, all or nothing. A denied take leaves
    /// the state unchanged and its `retry_after_ns` says when the same cost
    /// would fit. A cost of 0 takes nothing and only reports `remaining`;
    /// it is allowed unless a reconfigure has left more debt than the new
    /// burst.
    #[inline]
    pub fn take(&self, now_ns: u64, cost: u64) -> Result<Decision, Error> {
        let (iv, tol) = self.config();
        if cost > tol / iv {
            return Err(Error::CostExceedsBurst);
        }
        let mut d = Decision::default();
        if let Some((tag, now_off, mut word)) = self.fast_start(now_ns) {
            while word & PARITY == tag {
                match step::take(word & OFFSET_MAX, now_off, cost, iv, tol, &mut d) {
                    None => return Ok(d),
                    Some(next) if next <= OFFSET_MAX => match self.tat.compare_exchange_weak(
                        word,
                        tag | next,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return Ok(d),
                        Err(current) => word = current,
                    },
                    Some(_) => break,
                }
            }
        }
        let (wiv, wtol) = (u128::from(iv), u128::from(tol));
        Ok(self.update(
            now_ns,
            move |tat, now| {
                let mut d = Decision::default();
                (step::take(tat, now, cost, iv, tol, &mut d), d)
            },
            move |tat, now| {
                let mut d = Decision::default();
                (wide::take(tat, now, cost, wiv, wtol, &mut d), d)
            },
        ))
    }

    /// Tokens available at `now_ns` without taking any.
    #[inline]
    pub fn remaining(&self, now_ns: u64) -> u64 {
        let (iv, tol) = self.config();
        let (now_epoch, now_off) = clock::split(now_ns);
        let (base, word) = self.snapshot();
        let (epoch, off) = decode(base, word);
        if epoch == now_epoch {
            return step::avail(off.saturating_sub(now_off), tol, iv);
        }
        let tat = clock::epoch_start(epoch) + u128::from(off);
        let debt = tat.saturating_sub(clock::ticks_wide(now_ns));
        wide::avail(debt, u128::from(tol), u128::from(iv))
    }

    /// Take as many tokens as are available, up to `max`. When nothing is
    /// granted, `retry_after_ns` says when one token will be ready. Used for
    /// byte shaping, where a read is sized by the grant and the unused part
    /// handed back with [`Gcra::refund`].
    #[inline]
    pub fn take_upto(&self, now_ns: u64, max: u64) -> Decision {
        self.take_range(now_ns, 1, max)
    }

    /// Like [`Gcra::take_upto`], but grant nothing unless at least `min`
    /// tokens are available; then `retry_after_ns` says when `min` will be.
    /// Several consumers sharing one limiter use a `min` so that none of
    /// them can keep the others out by taking every token the moment it
    /// appears. A `min` above the burst is lowered to the burst.
    #[inline]
    pub fn take_range(&self, now_ns: u64, min: u64, max: u64) -> Decision {
        let (iv, tol) = self.config();
        let fast = move |tat, now, d: &mut Decision| {
            step::range(tat, now, min, max, iv, tol, d).map(|(_, n)| n)
        };
        if let Some(d) = self.update_fast(now_ns, fast) {
            return d;
        }
        let (wiv, wtol) = (u128::from(iv), u128::from(tol));
        self.update(
            now_ns,
            move |tat, now| {
                let mut d = Decision::default();
                (
                    step::range(tat, now, min, max, iv, tol, &mut d).map(|(_, n)| n),
                    d,
                )
            },
            move |tat, now| {
                let mut d = Decision::default();
                (
                    wide::range(tat, now, min, max, wiv, wtol, &mut d).map(|(_, n)| n),
                    d,
                )
            },
        )
    }

    /// Give back `tokens` previously granted and not used. The state never
    /// moves before `now_ns`, so a refund cannot create more than a full
    /// bucket.
    #[inline]
    pub fn refund(&self, now_ns: u64, tokens: u64) {
        let iv = self.interval.load(Ordering::Relaxed);
        let fast = move |tat, now, _: &mut Decision| {
            let next = step::refund(tat, now, tokens, iv);
            (next != tat).then_some(next)
        };
        if self.update_fast(now_ns, fast).is_some() {
            return;
        }
        let wiv = u128::from(iv);
        self.update(
            now_ns,
            move |tat, now| {
                let next = step::refund(tat, now, tokens, iv);
                ((next != tat).then_some(next), Decision::default())
            },
            move |tat, now| {
                let next = wide::refund(tat, now, tokens, wiv);
                ((next != tat).then_some(next), Decision::default())
            },
        );
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

    /// The theoretical arrival time in ticks since the clock's start, for
    /// differential tests and state handoff. It needs more than 64 bits
    /// after about 9 years of uptime.
    pub fn tat(&self) -> u128 {
        let (base, word) = self.snapshot();
        let (epoch, off) = decode(base, word);
        clock::epoch_start(epoch) + u128::from(off)
    }

    /// The limiter's base epoch: its arrival time counts from this epoch or
    /// the next.
    pub fn base_epoch(&self) -> u64 {
        self.base.load(Ordering::Acquire)
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
    use crate::clock::EPOCH_NS;
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
    fn every_u64_timestamp_works() {
        // libpacer clamped here; the epoch keeps every timestamp exact.
        let g = gcra(1, SEC, 1);
        assert!(allowed(&g, u64::MAX - SEC, 1));
        let d = g.take(u64::MAX - SEC / 2, 1).unwrap();
        assert!(!d.allowed);
        assert_eq!(d.retry_after_ns, SEC / 2);
        assert!(allowed(&g, u64::MAX, 1));
        assert_eq!(
            g.tat(),
            clock::ticks_wide(u64::MAX) + u128::from(g.interval())
        );
    }

    #[test]
    fn decisions_carry_across_an_epoch_boundary() {
        // 10/s, burst 5, drained 250 ms before the first epoch ends.
        let g = gcra(10, SEC, 5);
        let t0 = EPOCH_NS - 250 * MS;
        assert!(allowed(&g, t0, 5));
        assert_eq!(g.take(t0, 1).unwrap().retry_after_ns, 100 * MS);
        assert_eq!(g.remaining(EPOCH_NS - 1), 2);
        assert_eq!(g.remaining(EPOCH_NS), 2);
        let d = g.take(EPOCH_NS, 2).unwrap();
        assert_eq!((d.allowed, d.remaining), (true, 0));
        assert_eq!(g.take(EPOCH_NS, 1).unwrap().retry_after_ns, 50 * MS);
        assert_eq!(g.tat(), clock::ticks_wide(t0 + 700 * MS));
        assert!(!allowed(&g, EPOCH_NS + 50 * MS - 1, 1));
        assert!(allowed(&g, EPOCH_NS + 50 * MS, 1));
        // The arrival time is stored from the new epoch once it has to be.
        assert_eq!(g.base_epoch(), 0);
        assert_eq!(decode(0, g.tat.load(Ordering::Relaxed)).0, 0);
        assert!(allowed(&g, 2 * EPOCH_NS + SEC, 5));
        assert_eq!(decode(0, g.tat.load(Ordering::Relaxed)).0, 1);
        assert_eq!(g.base_epoch(), 0);
    }

    #[test]
    fn a_century_idle_catches_up_in_one_call() {
        const YEAR: u64 = 31_557_600 * SEC;
        let g = gcra(10, SEC, 5);
        assert!(allowed(&g, SEC, 5));
        assert_eq!(g.remaining(100 * YEAR), 5);
        assert_eq!(g.take_upto(100 * YEAR, 9).granted, 5);
        // The word counts from the epoch before now's, the base from the one before that.
        assert_eq!(g.base_epoch(), clock::epoch_of(100 * YEAR) - 2);
        assert_eq!(
            g.tat(),
            clock::ticks_wide(100 * YEAR) + 5 * u128::from(g.interval())
        );
        let d = g.take(100 * YEAR + 150 * MS, 2).unwrap();
        assert_eq!((d.allowed, d.retry_after_ns), (false, 50 * MS));
        g.refund(100 * YEAR + 150 * MS, 3);
        assert_eq!(g.remaining(100 * YEAR + 150 * MS), 4);
    }

    #[test]
    fn a_refund_back_in_time_moves_the_word_to_the_base_epoch() {
        // libpacer's last valid nanosecond puts the arrival time past 2^63
        // ticks, so the word counts from epoch 1; a refund at 5 s brings it
        // back to epoch 0, exactly as absolute arithmetic would.
        let g = gcra(1, SEC, 1);
        let late = MAX_NOW_NS - 1;
        assert!(allowed(&g, late, 1));
        assert_eq!(g.tat(), clock::ticks_wide(late) + u128::from(g.interval()));
        assert_eq!(decode(0, g.tat.load(Ordering::Relaxed)).0, 1);
        assert_eq!(g.remaining(5 * SEC), 0);
        g.refund(5 * SEC, u64::MAX);
        assert_eq!(g.tat(), clock::ticks_wide(5 * SEC));
        assert_eq!(decode(0, g.tat.load(Ordering::Relaxed)).0, 0);
        assert_eq!(g.remaining(5 * SEC), 1);
        assert!(allowed(&g, 5 * SEC, 1));
        assert_eq!(g.base_epoch(), 0);
    }

    #[test]
    fn a_refund_before_the_base_epoch_counts_from_its_start() {
        // Three epochs in, the base moves to 1 and the word to epoch 2.
        let g = gcra(1, SEC, 1);
        let now = 3 * EPOCH_NS;
        assert!(allowed(&g, now, 1));
        assert_eq!(g.base_epoch(), 1);
        assert_eq!(decode(1, g.tat.load(Ordering::Relaxed)).0, 2);
        assert_eq!(g.tat(), clock::ticks_wide(now) + u128::from(g.interval()));
        // A timestamp more than an epoch behind breaks the contract: the
        // arrival time stops at the base epoch's start, not at 10 ns.
        g.refund(10, u64::MAX);
        assert_eq!(g.tat(), clock::epoch_start(1));
        assert_eq!(g.remaining(10), 0);
        assert_eq!(g.remaining(EPOCH_NS), 1);
        // Back in the base epoch itself, the same holds.
        let g = gcra(1, SEC, 1);
        assert!(allowed(&g, now, 1));
        g.refund(EPOCH_NS - 1, 0);
        g.refund(now - 2 * EPOCH_NS + 1, u64::MAX);
        assert_eq!(g.tat(), clock::ticks_wide(now - 2 * EPOCH_NS + 1));
        assert!(allowed(&g, now - 2 * EPOCH_NS + 1, 1));
        let w = g.tat.load(Ordering::Relaxed);
        assert_eq!(decode(1, w).0, 1);
        g.refund(1, u64::MAX);
        let tat = g.tat();
        assert!(tat >= clock::epoch_start(1), "{tat}");
    }

    #[test]
    fn placing_an_arrival_time_picks_the_lowest_epoch_that_holds_it() {
        let u = u128::from(EPOCH_TICKS);
        assert_eq!(place(0, 0, 5), Some(5));
        assert_eq!(place(0, 0, u128::from(OFFSET_MAX)), Some(OFFSET_MAX));
        assert_eq!(place(0, 0, 2 * u), Some(encode(1, EPOCH_TICKS)));
        assert_eq!(place(0, 1, 3 * u - 1), Some(encode(1, OFFSET_MAX)));
        assert_eq!(place(0, 1, 3 * u), None);
        assert_eq!(place(4, 5, 4 * u + 7), Some(encode(4, 7)));
        assert_eq!(place(4, 5, 3 * u), Some(encode(4, 0)));
        assert_eq!(place(4, 4, 3 * u), Some(encode(4, 0)));
        for base in [0, 1, 7, 254] {
            for word in [0, 1, OFFSET_MAX, PARITY, u64::MAX] {
                let (e, off) = decode(base, word);
                assert!(e == base || e == base + 1);
                assert_eq!(encode(e, off), word);
            }
        }
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
        assert_eq!(g.tat(), clock::ticks_wide(SEC) + u128::from(g.interval()));
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

    #[test]
    fn concurrent_takes_across_epochs_never_over_grant() {
        // Eight threads race at instants straddling the start of epoch 3 on
        // a limiter last written in epoch 0, so the base moves while they
        // take. At 1/s nothing refills in those nanoseconds: exactly the
        // burst is granted.
        for round in 0..20u64 {
            let g = Arc::new(gcra(1, SEC, 4_000));
            assert!(g.take(round, 0).unwrap().allowed);
            let granted = Arc::new(AtomicU64::new(0));
            let threads: Vec<_> = (0..8u64)
                .map(|i| {
                    let (g, granted) = (Arc::clone(&g), Arc::clone(&granted));
                    std::thread::spawn(move || {
                        for j in 0..1_000u64 {
                            let now = 3 * EPOCH_NS - 2 + (i + j) % 4;
                            let got = if j % 3 == 0 {
                                g.take_upto(now, 2).granted
                            } else {
                                g.take(now, 1).unwrap().granted
                            };
                            granted.fetch_add(got, Ordering::Relaxed);
                        }
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
            assert_eq!(granted.load(Ordering::Relaxed), 4_000, "round {round}");
            assert_eq!(g.remaining(3 * EPOCH_NS + 1), 0);
            assert_eq!(g.base_epoch(), 1);
        }
    }
}
