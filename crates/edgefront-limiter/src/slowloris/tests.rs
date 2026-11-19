use std::time::Duration;

use super::*;

const SEC: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;

fn min(bytes: u64, window: Duration) -> MinRate {
    MinRate { bytes, window }
}

#[test]
fn a_check_is_a_fixed_small_value() {
    assert_eq!(std::mem::size_of::<SlowCheck>(), 184);
}

#[test]
fn nothing_to_check_without_a_minimum() {
    assert_eq!(SlowCheck::new(&min(0, Duration::from_secs(2)), SEC), None);
    // A window that cannot be cut into ten buckets.
    assert_eq!(
        SlowCheck::new(&min(100, Duration::from_nanos(9)), SEC),
        None
    );
    assert!(SlowCheck::new(&min(100, Duration::from_nanos(10)), SEC).is_some());
}

// it_timeouts.c: timeout_slowloris_is_cut_off. One byte every 200 ms is 5
// bytes per second, far below 100 bytes per 2 s; the check starts after the
// first window and cuts the connection there.
#[test]
fn a_trickling_client_is_cut_after_the_first_window() {
    let m = min(100, Duration::from_secs(2));
    let mut c = SlowCheck::new(&m, SEC).unwrap();
    assert_eq!(c.next_check(), 3 * SEC);
    let mut t = SEC;
    while t < 3 * SEC {
        c.moved(t, 1);
        assert_eq!(c.check(t, Some(&m)), Check::Pass { next_ns: 3 * SEC });
        t += 200 * MS;
    }
    assert_eq!(c.check(3 * SEC, Some(&m)), Check::Cut);
}

// it_timeouts.c: timeout_client_at_the_minimum_rate_survives. 64 bytes
// every 250 ms is 512 bytes per 2 s.
#[test]
fn a_client_above_the_minimum_survives_every_check() {
    let m = min(100, Duration::from_secs(2));
    let mut c = SlowCheck::new(&m, 0).unwrap();
    let mut checks = 0;
    let mut t = 0;
    while t <= 30 * SEC {
        if t % (250 * MS) == 0 {
            c.moved(t, 64);
        }
        if t >= c.next_check() {
            match c.check(t, Some(&m)) {
                Check::Pass { next_ns } => assert_eq!(next_ns, t + 200 * MS),
                other => panic!("{other:?} at {t}"),
            }
            checks += 1;
        }
        t += 10 * MS;
    }
    // Every tenth of a window from the end of the first: (30 - 2) / 0.2,
    // plus the first.
    assert_eq!(checks, 141);
}

#[test]
fn the_threshold_is_exact() {
    let m = min(100, Duration::from_secs(1));
    let mut c = SlowCheck::new(&m, 0).unwrap();
    c.moved(500 * MS, 99);
    assert_eq!(c.check(SEC, Some(&m)), Check::Cut);
    let mut c = SlowCheck::new(&m, 0).unwrap();
    c.moved(500 * MS, 100);
    assert_eq!(
        c.check(SEC, Some(&m)),
        Check::Pass {
            next_ns: SEC + 100 * MS
        }
    );
}

#[test]
fn bytes_that_left_the_window_no_longer_count() {
    let m = min(100, Duration::from_secs(1));
    let mut c = SlowCheck::new(&m, 0).unwrap();
    // A burst at 150 ms, in the bucket from 100 to 200 ms, then silence.
    c.moved(150 * MS, 1000);
    assert!(matches!(c.check(SEC, Some(&m)), Check::Pass { .. }));
    // At 1.1 s the window is (0.1 s, 1.1 s]: the whole bucket counts.
    assert!(matches!(c.check(1_100 * MS, Some(&m)), Check::Pass { .. }));
    // At 1.2 s it has left the window.
    assert_eq!(c.check(1_200 * MS, Some(&m)), Check::Cut);
}

// it_timeouts.c: timeout_shaped_client_is_not_called_slow. Our shaping
// holds the client below its minimum; it is not cut for that.
#[test]
fn a_client_we_throttled_in_the_last_window_is_exempt() {
    let m = min(1024, Duration::from_secs(1));
    let mut c = SlowCheck::new(&m, 0).unwrap();
    let mut t = 0;
    while t < 5 * SEC {
        // 512 bytes a second, each read followed by a throttle until the
        // next.
        c.moved(t, 512);
        c.throttled(t + SEC);
        if t >= c.next_check() {
            assert!(matches!(c.check(t, Some(&m)), Check::Pass { .. }), "{t}");
        }
        t += 100 * MS;
    }
    // Once the throttle is a whole window old, the exemption ends.
    let mut c = SlowCheck::new(&m, 0).unwrap();
    c.throttled(200 * MS);
    assert!(matches!(c.check(SEC, Some(&m)), Check::Pass { .. }));
    assert!(matches!(c.check(1_100 * MS, Some(&m)), Check::Pass { .. }));
    assert_eq!(c.check(1_200 * MS, Some(&m)), Check::Cut);
}

#[test]
fn an_earlier_throttle_never_moves_the_latest_back() {
    let m = min(10, Duration::from_secs(1));
    let mut c = SlowCheck::new(&m, 0).unwrap();
    c.throttled(900 * MS);
    c.throttled(100 * MS);
    assert!(matches!(c.check(1_800 * MS, Some(&m)), Check::Pass { .. }));
}

#[test]
fn a_reload_applies_at_the_next_check() {
    let m = min(100, Duration::from_secs(1));
    let mut c = SlowCheck::new(&m, 0).unwrap();
    c.moved(500 * MS, 50);
    // A lower minimum: 50 is enough now.
    let lower = min(50, Duration::from_secs(1));
    assert!(matches!(c.check(SEC, Some(&lower)), Check::Pass { .. }));
    // A longer window sets the cadence; the buckets keep their width.
    let longer = min(10, Duration::from_secs(5));
    assert_eq!(
        c.check(1_100 * MS, Some(&longer)),
        Check::Pass {
            next_ns: 1_600 * MS
        }
    );
    // Switched off, or set to 0 bytes: no more checks.
    let mut d = c.clone();
    assert_eq!(c.check(1_600 * MS, None), Check::Off);
    assert_eq!(
        d.check(1_600 * MS, Some(&min(0, Duration::from_secs(1)))),
        Check::Off
    );
}

#[test]
fn a_check_before_it_is_due_changes_nothing() {
    let m = min(100, Duration::from_secs(1));
    let mut c = SlowCheck::new(&m, 0).unwrap();
    let before = c.clone();
    assert_eq!(c.check(SEC - 1, Some(&m)), Check::Pass { next_ns: SEC });
    assert_eq!(c, before);
    // Even with the minimum switched off: that is noticed when due.
    assert_eq!(c.check(SEC - 1, None), Check::Pass { next_ns: SEC });
}

#[test]
fn deadlines_saturate_at_the_end_of_the_clock() {
    let m = min(1, Duration::from_secs(1));
    let mut c = SlowCheck::new(&m, u64::MAX - 10).unwrap();
    assert_eq!(c.next_check(), u64::MAX);
    c.moved(u64::MAX, 1);
    assert_eq!(
        c.check(u64::MAX, Some(&m)),
        Check::Pass { next_ns: u64::MAX }
    );
    let huge = min(1, Duration::MAX);
    let c = SlowCheck::new(&huge, 5).unwrap();
    assert_eq!(c.next_check(), u64::MAX);
}

/// A hashed timer wheel, as an event loop keeps one: `slots` buckets of
/// `tick` nanoseconds, each holding the (deadline, connection, kind) it
/// fires. Entries a whole turn or more ahead stay in their bucket until
/// their turn comes.
struct Wheel {
    tick: u64,
    slots: Vec<Vec<(u64, u32, Kind)>>,
    now: u64,
    len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Send,
    Check,
}

impl Wheel {
    fn new(tick: u64, slots: usize, now: u64) -> Self {
        Self {
            tick,
            slots: vec![Vec::new(); slots],
            now,
            len: 0,
        }
    }

    fn schedule(&mut self, at: u64, id: u32, kind: Kind) {
        let at = at.max(self.now);
        let n = self.slots.len() as u64;
        self.slots[((at / self.tick) % n) as usize].push((at, id, kind));
        self.len += 1;
    }

    /// Advance one tick and return what fell due, in deadline order.
    fn turn(&mut self, out: &mut Vec<(u64, u32, Kind)>) {
        self.now += self.tick;
        let n = self.slots.len() as u64;
        let slot = &mut self.slots[((self.now / self.tick - 1) % n) as usize];
        let now = self.now;
        let mut i = 0;
        while i < slot.len() {
            if slot[i].0 < now {
                out.push(slot.swap_remove(i));
                self.len -= 1;
            } else {
                i += 1;
            }
        }
        out.sort_unstable();
    }
}

/// 100,000 connections under `min_rate 1k per 2s`, on one timer wheel of
/// 10 ms ticks, for 8 virtual seconds: a third trickle 16 bytes every
/// 500 ms, a third send 512 bytes every 250 ms, and a third send 128
/// bytes every 250 ms but are throttled by shaping after every send. Every
/// trickler is cut within one check period of the end of its first window
/// (each opened at a different moment of the first second), nobody else
/// is cut, and each connection is checked once per tenth of a window and
/// no more.
#[test]
fn a_timer_wheel_of_100000_connections_cuts_exactly_the_tricklers() {
    const N: u32 = 100_000;
    const TICK: u64 = 10 * MS;
    const END: u64 = 9 * SEC;
    let m = min(1024, Duration::from_secs(2));
    let mut wheel = Wheel::new(TICK, 64, 0);
    let mut conns: Vec<Option<SlowCheck>> = Vec::with_capacity(N as usize);
    let mut opened = Vec::with_capacity(N as usize);
    let mut cut_at = vec![0u64; N as usize];
    let mut checks = vec![0u32; N as usize];
    for id in 0..N {
        // Spread the opens over the first second, 10 µs apart.
        let at = u64::from(id) * 10_000;
        let c = SlowCheck::new(&m, at).unwrap();
        wheel.schedule(c.next_check(), id, Kind::Check);
        wheel.schedule(at, id, Kind::Send);
        conns.push(Some(c));
        opened.push(at);
    }
    let mut due = Vec::new();
    while wheel.now < END {
        due.clear();
        wheel.turn(&mut due);
        for &(at, id, kind) in &due {
            let i = id as usize;
            let Some(c) = conns[i].as_mut() else {
                continue;
            };
            match kind {
                Kind::Send => {
                    let (bytes, every, shaped) = match id % 3 {
                        0 => (16, 500 * MS, false),
                        1 => (512, 250 * MS, false),
                        _ => (128, 250 * MS, true),
                    };
                    c.moved(at, bytes);
                    if shaped {
                        c.throttled(at + every);
                    }
                    wheel.schedule(at + every, id, Kind::Send);
                }
                Kind::Check => {
                    checks[i] += 1;
                    match c.check(at, Some(&m)) {
                        Check::Pass { next_ns } => wheel.schedule(next_ns, id, Kind::Check),
                        Check::Cut => {
                            cut_at[i] = at;
                            conns[i] = None;
                        }
                        Check::Off => unreachable!(),
                    }
                }
            }
        }
    }
    for id in 0..N as usize {
        let first = opened[id] + 2 * SEC;
        if id % 3 == 0 {
            assert!(
                cut_at[id] >= first && cut_at[id] < first + TICK,
                "connection {id} opened at {} cut at {}",
                opened[id],
                cut_at[id]
            );
            assert_eq!(checks[id], 1, "connection {id}");
        } else {
            assert_eq!(cut_at[id], 0, "connection {id} was cut");
            // From its first check to END, one per 200 ms (each fires up
            // to a tick late, which can cost one).
            let expect = (END - first) / (200 * MS);
            assert!(
                u64::from(checks[id]).abs_diff(expect) <= 1 + (END - first) / (20 * SEC),
                "connection {id}: {} checks, expected about {expect}",
                checks[id]
            );
        }
    }
    // Only the survivors' timers are left.
    assert_eq!(conns.iter().filter(|c| c.is_some()).count(), 66_666);
    assert!(wheel.len <= 2 * 66_666);
}
