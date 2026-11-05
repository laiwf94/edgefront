//! Monotonic time behind a trait, so balancing, health checking, outlier
//! detection and retry budgets can run against a clock that tests move by
//! hand. This is steer's `internal/clock`.
//!
//! [`MonotonicClock`] reads [`Instant::now`] and makes timers from tokio's
//! timer wheel. [`ManualClock`] stands still until [`ManualClock::advance`]
//! moves it, and then fires every timer that has come due, earliest
//! deadline first and, for equal deadlines, in the order they were made.
//! Code that needs time takes an `Arc<dyn Clock>`.
//!
//! As in steer, a timer resolves to the clock's reading when it fired: for
//! a manual clock that is the time the clock was advanced to, which can be
//! later than the timer's deadline.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, oneshot};

/// Tells the time and makes timers.
pub trait Clock: Send + Sync + fmt::Debug {
    /// The current reading. It never goes backwards.
    fn now(&self) -> Instant;

    /// A timer that fires once `d` has passed on this clock. A zero
    /// duration fires at once. A duration too long to represent never
    /// fires.
    fn after(&self, d: Duration) -> Timer;
}

/// The system's monotonic clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct MonotonicClock;

impl MonotonicClock {
    /// The monotonic clock as the shared handle code takes.
    pub fn shared() -> Arc<dyn Clock> {
        Arc::new(MonotonicClock)
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn after(&self, d: Duration) -> Timer {
        let now = Instant::now();
        if d.is_zero() {
            return Timer(State::Ready(now));
        }
        Timer(State::Monotonic {
            deadline: now.checked_add(d),
            sleep: None,
        })
    }
}

/// A one-shot timer from a [`Clock`]. Awaiting it yields the clock's
/// reading when it fired. Once that reading has been taken, by awaiting or
/// by [`Timer::try_fired`], the timer is spent and never resolves again,
/// like a drained channel.
#[must_use = "a timer does nothing unless it is awaited or checked"]
pub struct Timer(State);

enum State {
    /// Fired when it was made.
    Ready(Instant),
    /// `deadline` is `None` when it lies past what `Instant` can hold. The
    /// tokio sleep is made on first poll, so a timer can be made outside a
    /// runtime and awaited inside one.
    Monotonic {
        deadline: Option<Instant>,
        sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    },
    Manual(oneshot::Receiver<Instant>),
    Spent,
}

impl fmt::Debug for Timer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match &self.0 {
            State::Ready(_) => "ready",
            State::Monotonic { .. } => "monotonic",
            State::Manual(_) => "manual",
            State::Spent => "spent",
        };
        f.debug_tuple("Timer").field(&state).finish()
    }
}

impl Timer {
    /// The firing time if the timer has fired and its reading has not been
    /// taken yet, without waiting.
    pub fn try_fired(&mut self) -> Option<Instant> {
        let fired = match &mut self.0 {
            State::Ready(at) => Some(*at),
            State::Monotonic { deadline, .. } => {
                let now = Instant::now();
                deadline.is_some_and(|d| d <= now).then_some(now)
            }
            State::Manual(rx) => rx.try_recv().ok(),
            State::Spent => None,
        };
        if fired.is_some() {
            self.0 = State::Spent;
        }
        fired
    }
}

impl Future for Timer {
    type Output = Instant;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Instant> {
        let fired = match &mut self.0 {
            State::Ready(at) => *at,
            State::Monotonic { deadline: None, .. } | State::Spent => return Poll::Pending,
            State::Monotonic {
                deadline: Some(deadline),
                sleep,
            } => {
                let deadline = tokio::time::Instant::from_std(*deadline);
                let sleep =
                    sleep.get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));
                match sleep.as_mut().poll(cx) {
                    Poll::Ready(()) => Instant::now(),
                    Poll::Pending => return Poll::Pending,
                }
            }
            State::Manual(rx) => match Pin::new(rx).poll(cx) {
                Poll::Ready(Ok(at)) => at,
                // The clock was dropped with this timer pending: it can
                // never be advanced again, so the timer never fires.
                Poll::Ready(Err(_)) => {
                    self.0 = State::Spent;
                    return Poll::Pending;
                }
                Poll::Pending => return Poll::Pending,
            },
        };
        self.0 = State::Spent;
        Poll::Ready(fired)
    }
}

/// A clock that only moves when told to. It is safe to share between
/// threads and tasks.
#[derive(Debug)]
pub struct ManualClock {
    start: Instant,
    inner: Mutex<Inner>,
    /// Woken each time a timer is made, for [`ManualClock::wait_for_waiters`].
    made: Notify,
}

#[derive(Debug)]
struct Inner {
    now: Instant,
    /// Pending timers sorted by deadline, ties in the order they were
    /// made; `None` (never) sorts last.
    waiters: Vec<Waiter>,
}

#[derive(Debug)]
struct Waiter {
    at: Option<Instant>,
    tx: oneshot::Sender<Instant>,
}

impl Default for ManualClock {
    fn default() -> Self {
        ManualClock::new()
    }
}

impl ManualClock {
    /// A manual clock reading the present moment.
    pub fn new() -> Self {
        ManualClock::starting_at(Instant::now())
    }

    /// A manual clock reading `start`.
    pub fn starting_at(start: Instant) -> Self {
        ManualClock {
            start,
            inner: Mutex::new(Inner {
                now: start,
                waiters: Vec::new(),
            }),
            made: Notify::new(),
        }
    }

    /// The reading it started at.
    pub fn start(&self) -> Instant {
        self.start
    }

    /// How far the clock has been advanced in total.
    pub fn elapsed(&self) -> Duration {
        self.lock().now - self.start
    }

    /// Moves the clock forward by `d` and fires every timer that is now
    /// due, earliest deadline first. Returns how many fired.
    ///
    /// # Panics
    ///
    /// If the new reading lies past what `Instant` can hold.
    pub fn advance(&self, d: Duration) -> usize {
        let (now, due) = {
            let mut inner = self.lock();
            inner.now = inner
                .now
                .checked_add(d)
                .expect("manual clock advanced past the end of Instant");
            let now = inner.now;
            let n = inner
                .waiters
                .partition_point(|w| w.at.is_some_and(|at| at <= now));
            (now, inner.waiters.drain(..n).collect::<Vec<_>>())
        };
        // Wake outside the lock, so a woken task that touches the clock
        // from another thread never waits on it.
        let mut fired = 0;
        for w in due {
            if w.tx.send(now).is_ok() {
                fired += 1;
            }
        }
        fired
    }

    /// The number of timers made and not yet fired, leaving out any whose
    /// [`Timer`] was dropped.
    pub fn waiters(&self) -> usize {
        let mut inner = self.lock();
        inner.waiters.retain(|w| !w.tx.is_closed());
        inner.waiters.len()
    }

    /// Waits until at least `n` timers are pending or `timeout` passes on
    /// the runtime's timer, and reports whether the count was reached.
    /// Tests use it to know that a task is parked on the clock before they
    /// advance it.
    pub async fn wait_for_waiters(&self, n: usize, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let made = self.made.notified();
            tokio::pin!(made);
            // Register before counting, so a timer made in between still
            // wakes us.
            made.as_mut().enable();
            if self.waiters() >= n {
                return true;
            }
            if tokio::time::timeout_at(deadline, made).await.is_err() {
                return self.waiters() >= n;
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock leaves the state consistent: every
        // mutation above completes before anything can panic.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.lock().now
    }

    fn after(&self, d: Duration) -> Timer {
        let mut inner = self.lock();
        if d.is_zero() {
            return Timer(State::Ready(inner.now));
        }
        let at = inner.now.checked_add(d);
        let (tx, rx) = oneshot::channel();
        inner.waiters.retain(|w| !w.tx.is_closed());
        let i = match at {
            None => inner.waiters.len(),
            Some(at) => inner
                .waiters
                .partition_point(|w| w.at.is_some_and(|t| t <= at)),
        };
        inner.waiters.insert(i, Waiter { at, tx });
        drop(inner);
        self.made.notify_waiters();
        Timer(State::Manual(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use proptest::prelude::*;

    const SEC: Duration = Duration::from_secs(1);
    const MS: Duration = Duration::from_millis(1);

    fn assert_send_unpin<T: Send + Unpin + 'static>() {}

    #[test]
    fn timer_is_send_and_unpin() {
        assert_send_unpin::<Timer>();
    }

    // TestFakeFiresInOrder.
    #[test]
    fn manual_fires_in_order() {
        let f = ManualClock::new();
        let epoch = f.start();
        let mut late = f.after(2 * SEC);
        let mut early = f.after(SEC);
        assert_eq!(f.waiters(), 2);
        assert_eq!(f.advance(500 * MS), 0);
        assert_eq!(early.try_fired(), None, "fired early");
        assert_eq!(f.advance(500 * MS), 1);
        assert_eq!(early.try_fired(), Some(epoch + SEC));
        assert_eq!(late.try_fired(), None, "late fired too soon");
        assert_eq!(f.advance(Duration::from_secs(3600)), 1);
        assert!(late.try_fired().is_some());
        assert_eq!(f.waiters(), 0);
        assert_eq!(f.now(), epoch + Duration::from_secs(3601));
        assert_eq!(f.elapsed(), Duration::from_secs(3601));
    }

    // TestFakeZeroDurationFiresImmediately.
    #[test]
    fn manual_zero_duration_fires_immediately() {
        let f = ManualClock::new();
        assert_eq!(f.after(Duration::ZERO).try_fired(), Some(f.start()));
        assert_eq!(f.waiters(), 0);
    }

    // TestWaitForWaiters.
    #[tokio::test]
    async fn wait_for_waiters() {
        let f = Arc::new(ManualClock::new());
        assert!(!f.wait_for_waiters(1, 5 * MS).await, "no waiter yet");
        let g = Arc::clone(&f);
        let parked = tokio::spawn(async move { g.after(SEC).await });
        assert!(f.wait_for_waiters(1, 5 * SEC).await, "waiter not seen");
        f.advance(SEC);
        assert_eq!(parked.await.unwrap(), f.start() + SEC);
    }

    #[tokio::test]
    async fn wait_for_waiters_sees_timers_made_on_other_threads() {
        let f = Arc::new(ManualClock::new());
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let g = Arc::clone(&f);
                std::thread::spawn(move || g.after(SEC))
            })
            .collect();
        assert!(f.wait_for_waiters(4, 5 * SEC).await);
        let timers: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(f.waiters(), 4);
        assert_eq!(f.advance(SEC), 4);
        drop(timers);
    }

    // TestReal.
    #[tokio::test]
    async fn monotonic_timer_fires_and_time_moves_forward() {
        let c = MonotonicClock::shared();
        let before = c.now();
        let fired = c.after(MS).await;
        assert!(fired >= before + MS);
        assert!(c.now() >= fired);
    }

    #[tokio::test]
    async fn monotonic_zero_duration_is_ready() {
        let c = MonotonicClock;
        let before = Instant::now();
        let mut t = c.after(Duration::ZERO);
        assert!(t.try_fired().is_some_and(|at| at >= before));
        assert_eq!(t.try_fired(), None, "a spent timer stays spent");
        assert!(c.after(Duration::ZERO).await >= before);
    }

    #[test]
    fn monotonic_try_fired_after_the_deadline_without_a_runtime() {
        let mut t = MonotonicClock.after(MS);
        assert_eq!(format!("{t:?}"), "Timer(\"monotonic\")");
        let deadline = Instant::now() + 2 * MS;
        while Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(t.try_fired().is_some());
        assert_eq!(format!("{t:?}"), "Timer(\"spent\")");
        assert_eq!(t.try_fired(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn monotonic_timer_follows_tokio_time() {
        let c = MonotonicClock;
        let mut t = c.after(Duration::from_secs(30));
        assert!(futures_poll(&mut t).is_pending());
        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(futures_poll(&mut t).is_ready());
        assert!(futures_poll(&mut t).is_pending(), "spent");
    }

    #[test]
    fn durations_past_instant_never_fire() {
        let f = ManualClock::new();
        let mut never = f.after(Duration::MAX);
        let mut soon = f.after(SEC);
        assert_eq!(f.waiters(), 2);
        assert_eq!(f.advance(Duration::from_secs(86_400 * 365)), 1);
        assert!(soon.try_fired().is_some());
        assert_eq!(never.try_fired(), None);
        assert_eq!(f.waiters(), 1);

        let mut m = MonotonicClock.after(Duration::MAX);
        assert_eq!(m.try_fired(), None);
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(Pin::new(&mut m).poll(&mut cx).is_pending());
    }

    #[test]
    #[should_panic(expected = "advanced past the end of Instant")]
    fn advancing_past_instant_panics() {
        ManualClock::new().advance(Duration::MAX);
    }

    #[test]
    fn equal_deadlines_fire_in_the_order_they_were_made() {
        let f = Arc::new(ManualClock::new());
        let order = Arc::new(Mutex::new(Vec::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            // Made out of deadline order; ties at 2s in the order a, b, c.
            let spec = [("a", 2), ("x", 3), ("b", 2), ("e", 1), ("c", 2)];
            let tasks: Vec<_> = spec
                .iter()
                .map(|&(name, secs)| {
                    let timer = f.after(Duration::from_secs(secs));
                    let order = Arc::clone(&order);
                    tokio::spawn(async move {
                        let at = timer.await;
                        order.lock().unwrap().push((name, at));
                    })
                })
                .collect();
            // Let every task park on its timer.
            tokio::task::yield_now().await;
            assert_eq!(f.advance(Duration::from_secs(10)), 5);
            for t in tasks {
                t.await.unwrap();
            }
        });
        let order = order.lock().unwrap();
        let names: Vec<_> = order.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["e", "a", "b", "c", "x"]);
        let fired = f.start() + Duration::from_secs(10);
        assert!(order.iter().all(|&(_, at)| at == fired));
    }

    #[test]
    fn dropped_timers_are_not_counted_or_fired() {
        let f = ManualClock::new();
        let kept = f.after(SEC);
        drop(f.after(SEC));
        drop(f.after(2 * SEC));
        assert_eq!(f.waiters(), 1);
        drop(f.after(SEC));
        assert_eq!(f.advance(5 * SEC), 1);
        drop(kept);
    }

    #[tokio::test]
    async fn a_timer_whose_clock_is_dropped_never_fires() {
        let f = ManualClock::new();
        let mut t = f.after(SEC);
        assert_eq!(format!("{t:?}"), "Timer(\"manual\")");
        drop(f);
        assert!(futures_poll(&mut t).is_pending());
        assert!(futures_poll(&mut t).is_pending());
        assert_eq!(t.try_fired(), None);
    }

    #[test]
    fn manual_clock_is_a_shared_clock() {
        let f = Arc::new(ManualClock::default());
        let c: Arc<dyn Clock> = f.clone();
        let mut t = c.after(SEC);
        assert_eq!(format!("{:?}", c.after(Duration::ZERO)), "Timer(\"ready\")");
        f.advance(SEC);
        assert_eq!(c.now(), f.start() + SEC);
        assert_eq!(t.try_fired(), Some(c.now()));
    }

    // Threads make timers and wait on them while the main thread advances
    // the clock: every timer fires exactly once and none is lost.
    #[test]
    fn concurrent_timers_all_fire_once() {
        const THREADS: usize = 16;
        const PER_THREAD: u64 = 200;
        let f = Arc::new(ManualClock::new());
        let fired = Arc::new(AtomicUsize::new(0));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let tasks: Vec<_> = (0..THREADS)
                .map(|i| {
                    let f = Arc::clone(&f);
                    let fired = Arc::clone(&fired);
                    tokio::spawn(async move {
                        for k in 1..=PER_THREAD {
                            let d = Duration::from_millis(k * 7 + i as u64);
                            let at = f.after(d).await;
                            assert!(at >= f.start() + d, "fired before its deadline");
                            fired.fetch_add(1, Ordering::Relaxed);
                        }
                    })
                })
                .collect();
            while tasks.iter().any(|t| !t.is_finished()) {
                f.advance(MS);
                tokio::task::yield_now().await;
            }
            for t in tasks {
                t.await.unwrap();
            }
        });
        assert_eq!(fired.load(Ordering::Relaxed), THREADS * PER_THREAD as usize);
        assert_eq!(f.waiters(), 0);
    }

    fn futures_poll(t: &mut Timer) -> Poll<Instant> {
        let waker = std::task::Waker::noop();
        Pin::new(t).poll(&mut Context::from_waker(waker))
    }

    proptest! {
        // Each timer fires on the first advance that reaches its deadline,
        // reading the clock's time after that advance, and never before.
        #[test]
        fn timers_fire_on_the_advance_that_reaches_them(
            delays in prop::collection::vec(0u64..1_000, 1..40),
            steps in prop::collection::vec(0u64..200, 1..40),
        ) {
            let f = ManualClock::new();
            let mut timers: Vec<_> = delays
                .iter()
                .map(|&ms| (ms, f.after(Duration::from_millis(ms))))
                .collect();
            let mut elapsed = 0;
            for (ms, t) in &mut timers {
                if *ms == 0 {
                    prop_assert_eq!(t.try_fired(), Some(f.start()));
                }
            }
            for step in steps {
                let before = elapsed;
                elapsed += step;
                let due = timers.iter().filter(|(ms, _)| *ms > before && *ms <= elapsed).count();
                prop_assert_eq!(f.advance(Duration::from_millis(step)), due);
                for (ms, t) in &mut timers {
                    let fired = t.try_fired();
                    if *ms > before && *ms <= elapsed {
                        prop_assert_eq!(fired, Some(f.start() + Duration::from_millis(elapsed)));
                    } else {
                        prop_assert_eq!(fired, None);
                    }
                }
                let pending = timers.iter().filter(|(ms, _)| *ms > elapsed).count();
                prop_assert_eq!(f.waiters(), pending);
            }
        }
    }
}
