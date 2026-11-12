//! Sliding-window counter, ported from libpacer's `window.c`.
//!
//! A [`Window`] answers "how many events (bytes, requests) happened in the
//! last `window_ns`". The window is cut into `slots` buckets of
//! `window_ns / slots` nanoseconds, plus the bucket being filled. The
//! oldest bucket is weighted by how much of it is still inside the window,
//! so the count is within one bucket's worth of exact.
//!
//! A window belongs to one owner (a connection, a key under a lock): its
//! methods take `&mut self` and it holds no atomics. It is a plain value of
//! fixed size with no allocation, so it can be embedded anywhere.
//!
//! Counts are kept modulo 2^64, as in libpacer: the running total is
//! always the wrapping sum of the buckets, so adding and expiring stay
//! consistent even if a caller ever adds past `u64::MAX`.

use crate::gcra::Error;

/// The most buckets a window may be cut into.
pub const SLOTS_MAX: u32 = 16;

/// Buckets in the ring: the `slots` that make up the window plus the one
/// being filled.
const RING: usize = SLOTS_MAX as usize + 1;

/// A sliding-window event counter. See the [module docs](self).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    /// Width of one bucket in nanoseconds.
    slot_ns: u64,
    /// Start of the bucket being filled.
    head_start: u64,
    /// Wrapping sum of all buckets.
    total: u64,
    /// Buckets in use: `slots + 1`.
    n: u32,
    /// Index of the bucket being filled.
    head: u32,
    counts: [u64; RING],
}

impl Window {
    /// An empty window whose first bucket starts at `now_ns`. `slots` is 1
    /// to [`SLOTS_MAX`] and `window_ns` must be at least `slots`, so every
    /// bucket is at least a nanosecond wide. When `window_ns` is not a
    /// multiple of `slots` the bucket width is rounded down, and the window
    /// is `slots` times that width.
    pub fn new(window_ns: u64, slots: u32, now_ns: u64) -> Result<Self, Error> {
        if slots == 0 || slots > SLOTS_MAX || window_ns < u64::from(slots) {
            return Err(Error::InvalidWindow);
        }
        Ok(Self {
            slot_ns: window_ns / u64::from(slots),
            head_start: now_ns,
            total: 0,
            n: slots + 1,
            head: 0,
            counts: [0; RING],
        })
    }

    /// Move the ring forward so the head bucket contains `now_ns`. Times
    /// before the head bucket leave it where it is.
    fn advance(&mut self, now_ns: u64) {
        // The C compared now with head_start + slot_ns, which can wrap for
        // a window near the top of the clock; the difference cannot.
        if now_ns < self.head_start || now_ns - self.head_start < self.slot_ns {
            return;
        }
        let steps = (now_ns - self.head_start) / self.slot_ns;
        // steps * slot_ns <= now_ns - head_start, so neither step wraps.
        self.head_start += steps * self.slot_ns;
        if steps >= u64::from(self.n) {
            self.counts = [0; RING];
            self.total = 0;
            return;
        }
        for _ in 0..steps {
            self.head = (self.head + 1) % self.n;
            let expired = std::mem::take(&mut self.counts[self.head as usize]);
            self.total = self.total.wrapping_sub(expired);
        }
    }

    /// Record `n` events at `now_ns`. A time earlier than the newest seen
    /// is counted as the newest.
    pub fn add(&mut self, now_ns: u64, n: u64) {
        self.advance(now_ns);
        let head = &mut self.counts[self.head as usize];
        *head = head.wrapping_add(n);
        self.total = self.total.wrapping_add(n);
    }

    /// Events in `(now_ns - window, now_ns]`, with the oldest bucket
    /// weighted by the part of it still inside the window. Like
    /// [`Window::add`], it moves the window forward to `now_ns`.
    pub fn count(&mut self, now_ns: u64) -> u64 {
        self.advance(now_ns);
        // After advance, now_ns < head_start + slot_ns, so into < slot_ns.
        let into = now_ns.saturating_sub(self.head_start);
        let oldest = self.counts[((self.head + 1) % self.n) as usize];
        // The window starts `into` past the start of the oldest bucket, so
        // only (slot - into) / slot of it still counts.
        let keep = u128::from(oldest) * u128::from(self.slot_ns - into) / u128::from(self.slot_ns);
        // keep <= oldest, so it fits.
        self.total.wrapping_sub(oldest).wrapping_add(keep as u64)
    }

    /// How long after `now_ns` [`Window::count`] first returns at most `n`,
    /// if no more events are added: 0 when it already does. The count never
    /// rises as time passes, so this is the exact wait a caller denied for
    /// lack of room must sit out. Does not move the window.
    ///
    /// After `j` more bucket steps the `j` oldest buckets have gone, and
    /// within that step the new oldest bucket `b` is weighted by
    /// `(slot - into) / slot`, rounded down. That is at most `room` exactly
    /// when `b * (slot - into) < (room + 1) * slot`, which gives the first
    /// `into` in closed form, so the wait costs one pass over the ring.
    pub fn until_at_most(&self, now_ns: u64, n: u64) -> u64 {
        let mut w = self.clone();
        w.advance(now_ns);
        let slot = u128::from(w.slot_ns);
        let mut into = u128::from(now_ns.saturating_sub(w.head_start));
        if w.clone().count(now_ns) <= n {
            return 0;
        }
        let mut total = w.total;
        for j in 0..u128::from(w.n) {
            // j steps on, the bucket after the head is the oldest.
            let oldest = w.counts[((u128::from(w.head) + 1 + j) % u128::from(w.n)) as usize];
            let rest = total.wrapping_sub(oldest);
            if rest <= n {
                let room = u128::from(n - rest);
                let oldest = u128::from(oldest);
                // The first `into` with oldest * (slot - into) < (room + 1) * slot.
                let first = if oldest * (slot - into) < (room + 1) * slot {
                    into
                } else {
                    (slot + 1).saturating_sub(((room + 1) * slot).div_ceil(oldest))
                };
                if first < slot {
                    let at = u128::from(w.head_start) + j * slot + first.max(into);
                    return u64::try_from(at - u128::from(now_ns)).unwrap_or(u64::MAX);
                }
            }
            total = rest;
            into = 0;
        }
        // Every bucket has gone by now: the count is 0.
        let at = u128::from(w.head_start) + u128::from(w.n) * slot;
        u64::try_from(at - u128::from(now_ns)).unwrap_or(u64::MAX)
    }

    /// The width of one bucket in nanoseconds.
    pub fn slot_ns(&self) -> u64 {
        self.slot_ns
    }

    /// The number of buckets the window is cut into.
    pub fn slots(&self) -> u32 {
        self.n - 1
    }

    /// The length of the window in nanoseconds: `slots` times the bucket
    /// width.
    pub fn window_ns(&self) -> u64 {
        self.slot_ns * u64::from(self.slots())
    }

    /// Start of the bucket being filled, for differential tests.
    pub fn head_start(&self) -> u64 {
        self.head_start
    }

    /// Sum of all buckets, including the whole oldest one, for differential
    /// tests.
    pub fn total(&self) -> u64 {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000_000_000;
    const MS: u64 = 1_000_000;

    // Ported from libpacer's tests/test_window.c. The NULL-pointer case is
    // dropped: a `&mut Window` cannot be null.

    #[test]
    fn window_rejects_bad_config() {
        assert_eq!(Window::new(SEC, 0, 0), Err(Error::InvalidWindow));
        assert_eq!(
            Window::new(SEC, SLOTS_MAX + 1, 0),
            Err(Error::InvalidWindow)
        );
        assert_eq!(Window::new(3, 4, 0), Err(Error::InvalidWindow));
        assert!(Window::new(SEC, SLOTS_MAX, 0).is_ok());
        assert_eq!(
            Error::InvalidWindow.to_string(),
            "invalid window configuration"
        );
    }

    #[test]
    fn window_counts_recent_events() {
        let mut w = Window::new(10 * SEC, 10, 0).unwrap();
        w.add(0, 5);
        w.add(3 * SEC, 7);
        assert_eq!(w.count(3 * SEC), 12);
        assert_eq!(w.count(9 * SEC), 12);
    }

    #[test]
    fn window_forgets_old_events() {
        let mut w = Window::new(10 * SEC, 10, 0).unwrap();
        w.add(0, 100);
        // At 10 s the first bucket is the oldest and fully inside; by 11 s
        // it has slid out completely.
        assert_eq!(w.count(10 * SEC), 100);
        assert_eq!(w.count(10 * SEC + 500 * MS), 50);
        assert_eq!(w.count(11 * SEC), 0);
    }

    #[test]
    fn window_large_jump_clears_everything() {
        let mut w = Window::new(4 * SEC, 4, SEC).unwrap();
        for t in 0..4 {
            w.add(SEC + t * SEC, 10);
        }
        assert_eq!(w.count(4 * SEC), 40);
        assert_eq!(w.count(1000 * SEC), 0);
        w.add(1000 * SEC, 3);
        assert_eq!(w.count(1000 * SEC), 3);
    }

    #[test]
    fn window_ignores_time_going_backwards() {
        let mut w = Window::new(4 * SEC, 4, 10 * SEC).unwrap();
        w.add(10 * SEC, 1);
        w.add(5 * SEC, 1);
        assert_eq!(w.count(5 * SEC), 2);
    }

    #[test]
    fn window_stays_within_one_bucket_of_exact() {
        // A steady 1 event per ms against a 1 s window in 10 buckets: the
        // exact count is 1000 once warmed up; the estimate must be within
        // 100.
        let mut w = Window::new(SEC, 10, 0).unwrap();
        let mut worst = 0;
        for t in (0..5 * SEC).step_by(MS as usize) {
            w.add(t, 1);
            if t >= 2 * SEC {
                worst = worst.max(w.count(t).abs_diff(1000));
            }
        }
        assert!(worst <= 100, "worst error {worst}");
    }

    // Added in the port.

    #[test]
    fn accessors_describe_the_shape() {
        let w = Window::new(10 * SEC + 7, 3, 42).unwrap();
        assert_eq!(w.slots(), 3);
        assert_eq!(w.slot_ns(), (10 * SEC + 7) / 3);
        assert_eq!(w.window_ns(), (10 * SEC + 7) / 3 * 3);
        assert_eq!(w.head_start(), 42);
        assert_eq!(w.total(), 0);
        assert_eq!(std::mem::size_of::<Window>(), 168);
    }

    #[test]
    fn one_slot_window_at_the_top_of_the_clock() {
        // head_start + slot_ns would wrap here; the C compared against that
        // sum. The window must still advance and expire correctly.
        let mut w = Window::new(u64::MAX, 1, u64::MAX - 10).unwrap();
        w.add(u64::MAX - 10, 4);
        assert_eq!(w.count(u64::MAX), 4);
        let mut w = Window::new(100, 1, u64::MAX - 150).unwrap();
        w.add(u64::MAX - 150, 4);
        assert_eq!(w.count(u64::MAX - 51), 4);
        // One slot past: the bucket is now the oldest, weighted by 50/100.
        assert_eq!(w.count(u64::MAX), 2);
    }

    #[test]
    fn counts_wrap_like_the_c() {
        let mut w = Window::new(2, 2, 0).unwrap();
        w.add(0, u64::MAX);
        w.add(1, 2);
        assert_eq!(w.total(), 1);
        // Both buckets are inside at t=1; the sum wraps to 1.
        assert_eq!(w.count(1), 1);
        // At t=2 the first bucket is the oldest, fully inside.
        assert_eq!(w.count(2), 1);
        // At t=3 it has gone; only the 2 remain.
        assert_eq!(w.count(3), 2);
    }

    /// An exact reference: every event kept with the absolute bucket it
    /// fell into, and the documented estimate computed from them.
    struct Reference {
        start: u64,
        slot: u64,
        slots: u64,
        latest: u64,
        events: Vec<(u64, u64, u64)>, // (bucket, time it counts at, n)
    }

    impl Reference {
        fn new(window_ns: u64, slots: u32, now: u64) -> Self {
            Self {
                start: now,
                slot: window_ns / u64::from(slots),
                slots: u64::from(slots),
                latest: now,
                events: Vec::new(),
            }
        }

        fn bucket(&self, t: u64) -> u64 {
            (t - self.start) / self.slot
        }

        fn add(&mut self, now: u64, n: u64) {
            self.latest = self.latest.max(now);
            self.events.push((self.bucket(self.latest), self.latest, n));
        }

        /// The estimate and the exact count of events in (now - W, now],
        /// counting each event at the time it was recorded as.
        fn count(&mut self, now: u64) -> (u64, u64) {
            self.latest = self.latest.max(now);
            let head = self.bucket(self.latest);
            let head_start = self.start + head * self.slot;
            let into = u128::from(now.saturating_sub(head_start));
            let oldest_bucket = head.checked_sub(self.slots);
            let (mut total, mut oldest) = (0u128, 0u128);
            for &(b, _, n) in &self.events {
                if b + self.slots >= head {
                    total += u128::from(n);
                    if Some(b) == oldest_bucket {
                        oldest += u128::from(n);
                    }
                }
            }
            let slot = u128::from(self.slot);
            let estimate = total - oldest + oldest * (slot - into) / slot;
            let from = now.saturating_sub(self.slot * self.slots);
            let exact = self
                .events
                .iter()
                .filter(|&&(_, t, _)| t > from && t <= now)
                .map(|&(_, _, n)| u128::from(n))
                .sum::<u128>();
            (estimate as u64, exact as u64)
        }
    }

    fn splitmix(s: &mut u64) -> u64 {
        *s = s.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *s;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    #[test]
    fn matches_an_exact_reference_on_random_scripts() {
        for seed in 1..=200u64 {
            let mut s = seed;
            let slots = 1 + (splitmix(&mut s) % u64::from(SLOTS_MAX)) as u32;
            let window_ns = u64::from(slots) + splitmix(&mut s) % 10_000;
            let start = splitmix(&mut s) % 1_000_000;
            let mut w = Window::new(window_ns, slots, start).unwrap();
            let mut r = Reference::new(window_ns, slots, start);
            let slot = w.slot_ns();
            let mut now = start;
            for step in 0..1_000 {
                now = match splitmix(&mut s) % 10 {
                    0 => now.saturating_sub(splitmix(&mut s) % (2 * slot + 1)),
                    1 => now + splitmix(&mut s) % (40 * slot + 1),
                    2 => now,
                    _ => now + splitmix(&mut s) % (slot + 1),
                };
                if splitmix(&mut s).is_multiple_of(3) {
                    let (want, exact) = r.count(now);
                    let got = w.count(now);
                    assert_eq!(got, want, "seed {seed} step {step} count at {now}");
                    // Within one bucket's worth of the exact count: the
                    // estimate never exceeds the events in the window plus
                    // the oldest bucket, nor falls below those minus it.
                    // Only checked when time has not gone backwards, since a
                    // count at an earlier time still sees the newer events.
                    let bucket = w.counts[((w.head + 1) % w.n) as usize];
                    assert!(
                        now < r.latest || got.abs_diff(exact) <= bucket,
                        "seed {seed} step {step}: estimate {got} exact {exact} bucket {bucket}"
                    );
                } else {
                    let n = splitmix(&mut s) % 1_000;
                    r.add(now, n);
                    w.add(now, n);
                }
                assert_eq!(
                    w.total(),
                    w.counts[..w.n as usize].iter().sum::<u64>(),
                    "seed {seed} step {step}: total is the sum of the buckets"
                );
            }
        }
    }

    #[test]
    fn until_at_most_is_the_exact_wait() {
        let at = |w: &Window, t: u64| w.clone().count(t);
        for seed in 1..=300u64 {
            let mut s = seed;
            let slots = 1 + (splitmix(&mut s) % u64::from(SLOTS_MAX)) as u32;
            let window_ns = u64::from(slots) * (1 + splitmix(&mut s) % 1_000);
            let start = splitmix(&mut s) % 1_000_000;
            let mut w = Window::new(window_ns, slots, start).unwrap();
            let slot = w.slot_ns();
            let mut now = start;
            for step in 0..200 {
                now += splitmix(&mut s) % (3 * slot);
                if !splitmix(&mut s).is_multiple_of(4) {
                    w.add(now, splitmix(&mut s) % 50);
                    continue;
                }
                // A time just before the head bucket, now and then.
                let t = if step % 7 == 0 {
                    now.saturating_sub(slot / 2)
                } else {
                    now
                };
                let n = splitmix(&mut s) % (w.count(now) + 2);
                let before = w.clone();
                let wait = w.until_at_most(t, n);
                assert_eq!(w, before, "seed {seed} step {step}: the window moved");
                assert!(
                    at(&w, t + wait) <= n,
                    "seed {seed} step {step}: {} > {n} after {wait}",
                    at(&w, t + wait)
                );
                if wait > 0 {
                    assert!(
                        at(&w, t + wait - 1) > n,
                        "seed {seed} step {step}: a wait of {wait} is too long for {n}"
                    );
                }
                // The count never rises with time.
                let mut last = at(&w, t);
                for k in 1..=20 {
                    let c = at(&w, t + k * (window_ns + slot) / 20);
                    assert!(c <= last, "seed {seed} step {step}: count rose");
                    last = c;
                }
            }
        }
    }

    #[test]
    fn until_at_most_past_every_bucket_and_at_the_top_of_the_clock() {
        // A bucket too large to weight below 1 is only gone with the ring.
        let mut w = Window::new(4, 4, 0).unwrap();
        w.add(0, 1_000);
        assert_eq!(w.until_at_most(0, 0), 5);
        assert_eq!(w.clone().count(5), 0);
        assert_eq!(w.clone().count(4), 1_000);
        assert_eq!(w.until_at_most(0, 1_000), 0);
        let mut w = Window::new(100, 1, u64::MAX - 150).unwrap();
        w.add(u64::MAX - 150, 4);
        assert_eq!(w.until_at_most(u64::MAX - 150, 3), 101);
        let mut w = Window::new(u64::MAX, 1, u64::MAX - 10).unwrap();
        w.add(u64::MAX - 10, 4);
        assert_eq!(w.until_at_most(u64::MAX - 10, 0), u64::MAX);
    }
}
