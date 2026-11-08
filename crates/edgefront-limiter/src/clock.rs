//! Time for the limiters: the moving epoch that keeps arrival times in 64
//! bits for any uptime, and the clocks that feed them.
//!
//! libpacer kept a theoretical arrival time as an absolute count of 1/64 ns
//! ticks in one 64-bit word, so its clock stopped at 2^57 ns, about 4.5
//! years of uptime: past that every timestamp was clamped and limiters
//! stopped refilling. edgefront splits a timestamp into an *epoch*, the
//! number of whole 2^56 ns spans (about 2.28 years each) since the clock
//! started, and an offset inside it. A [`crate::Gcra`] stores its arrival
//! time as an offset from an epoch it records itself, and moves that epoch
//! forward (rebasing the offset) when the time it has to store no longer
//! fits. A `u64` of nanoseconds then lasts 584 years, and the decisions are
//! the same as exact arithmetic on absolute time.
//!
//! Every decision still takes `now_ns` from the caller. [`Clock`] is how a
//! caller gets it: [`MonotonicClock`] reads the operating system's
//! monotonic clock, and [`ManualClock`] is set by hand, for tests and
//! simulations that run years of uptime in microseconds.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::gcra::TICK_SHIFT;

/// Bits of nanoseconds inside one epoch.
pub const EPOCH_SHIFT: u32 = 56;
/// The length of an epoch in nanoseconds: 2^56 ns, about 2.28 years.
pub const EPOCH_NS: u64 = 1 << EPOCH_SHIFT;
/// The length of an epoch in ticks of 1/64 ns: 2^62, which is also the
/// deepest tolerance a rate may have ([`crate::gcra::MAX_TOL_TICKS`]).
pub const EPOCH_TICKS: u64 = 1 << (EPOCH_SHIFT + TICK_SHIFT);

/// The epoch `now_ns` falls in.
#[inline]
pub const fn epoch_of(now_ns: u64) -> u64 {
    now_ns >> EPOCH_SHIFT
}

/// `now_ns` as its epoch and its offset into that epoch in ticks. The
/// offset is below [`EPOCH_TICKS`].
#[inline]
pub const fn split(now_ns: u64) -> (u64, u64) {
    (
        now_ns >> EPOCH_SHIFT,
        (now_ns & (EPOCH_NS - 1)) << TICK_SHIFT,
    )
}

/// `now_ns` in ticks since the clock started. Needs 70 bits at most.
#[inline]
pub const fn ticks_wide(now_ns: u64) -> u128 {
    (now_ns as u128) << TICK_SHIFT
}

/// The start of `epoch` in ticks since the clock started.
#[inline]
pub const fn epoch_start(epoch: u64) -> u128 {
    (epoch as u128) * (EPOCH_TICKS as u128)
}

/// A source of monotonic timestamps in nanoseconds since some fixed start.
/// Timestamps never go backwards.
pub trait Clock: Send + Sync {
    fn now_ns(&self) -> u64;
}

/// The operating system's monotonic clock, counted from when this value
/// was made.
#[derive(Clone, Copy, Debug)]
pub struct MonotonicClock {
    start: Instant,
}

impl MonotonicClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MonotonicClock {
    fn now_ns(&self) -> u64 {
        // 584 years of uptime before this saturates.
        u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

/// A clock that only moves when told to, shared between threads. It never
/// goes backwards: [`ManualClock::set`] to an earlier time is ignored, and
/// [`ManualClock::advance`] saturates at `u64::MAX`.
#[derive(Debug, Default)]
pub struct ManualClock {
    now: AtomicU64,
}

impl ManualClock {
    pub const fn new(start_ns: u64) -> Self {
        Self {
            now: AtomicU64::new(start_ns),
        }
    }

    /// Move to `now_ns`, unless the clock is already past it. Returns the
    /// time the clock shows afterwards.
    pub fn set(&self, now_ns: u64) -> u64 {
        self.now.fetch_max(now_ns, Ordering::Relaxed).max(now_ns)
    }

    /// Move forward by `by_ns`. Returns the new time.
    pub fn advance(&self, by_ns: u64) -> u64 {
        let mut cur = self.now.load(Ordering::Relaxed);
        loop {
            let next = cur.saturating_add(by_ns);
            match self
                .now
                .compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return next,
                Err(seen) => cur = seen,
            }
        }
    }
}

impl Clock for ManualClock {
    fn now_ns(&self) -> u64 {
        self.now.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Nanoseconds in a Julian year.
    const YEAR: u64 = 31_557_600_000_000_000;

    #[test]
    fn an_epoch_is_about_two_and_a_quarter_years() {
        assert_eq!(EPOCH_NS, 72_057_594_037_927_936);
        const { assert!(EPOCH_NS > 2 * YEAR && EPOCH_NS < 23 * YEAR / 10) };
        assert_eq!(EPOCH_TICKS, crate::gcra::MAX_TOL_TICKS);
        // libpacer's horizon was two epochs.
        assert_eq!(crate::gcra::MAX_NOW_NS, 2 * EPOCH_NS);
        // A u64 of nanoseconds spans 256 epochs.
        assert_eq!(epoch_of(u64::MAX), 255);
    }

    #[test]
    fn split_is_exact_and_matches_wide_ticks() {
        for now in [
            0,
            1,
            EPOCH_NS - 1,
            EPOCH_NS,
            EPOCH_NS + 1,
            45 * YEAR / 10,
            10 * YEAR,
            100 * YEAR,
            u64::MAX,
        ] {
            let (e, off) = split(now);
            assert_eq!(e, epoch_of(now));
            assert!(off < EPOCH_TICKS, "{now}");
            assert_eq!(epoch_start(e) + u128::from(off), ticks_wide(now), "{now}");
        }
        assert_eq!(split(EPOCH_NS - 1), (0, EPOCH_TICKS - 64));
        assert_eq!(split(EPOCH_NS), (1, 0));
        assert_eq!(epoch_of(100 * YEAR), 43);
    }

    #[test]
    fn the_monotonic_clock_moves_forward() {
        let c = MonotonicClock::default();
        let a = c.now_ns();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = c.now_ns();
        assert!(b >= a + 1_000_000, "{a} {b}");
        let copy = c;
        assert!(copy.now_ns() >= b);
    }

    #[test]
    fn the_manual_clock_never_goes_backwards() {
        let c = ManualClock::new(10);
        assert_eq!(c.now_ns(), 10);
        assert_eq!(c.set(5), 10);
        assert_eq!(c.set(100 * YEAR), 100 * YEAR);
        assert_eq!(c.advance(7), 100 * YEAR + 7);
        assert_eq!(c.advance(u64::MAX), u64::MAX);
        assert_eq!(c.now_ns(), u64::MAX);
        assert_eq!(ManualClock::default().now_ns(), 0);
    }

    #[test]
    fn the_manual_clock_adds_up_across_threads() {
        let c = Arc::new(ManualClock::new(0));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let c = Arc::clone(&c);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        c.advance(3);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(c.now_ns(), 12_000);
        let dyn_clock: &dyn Clock = &*c;
        assert_eq!(dyn_clock.now_ns(), 12_000);
    }
}
