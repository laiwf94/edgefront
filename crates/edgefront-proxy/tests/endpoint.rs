//! Endpoint state against a line-for-line model of steer's
//! `endpoint.State`: random sequences of probes, results, ejections,
//! decays and clears, at random times, must leave both with the same
//! verdict, counters, multiplier and ejection end.

use std::time::{Duration, Instant};

use edgefront_proxy::endpoint::State;
use proptest::prelude::*;

/// steer's state machine, transcribed with Go's integer types and its
/// one-period-at-a-time decay loop. Times are nanoseconds since the epoch.
#[derive(Debug, Default)]
struct Model {
    healthy: bool,
    ejected_until: i64,
    /// Whether `ejected_until` holds an ejection's end. steer keeps Unix
    /// nanoseconds and uses zero for "none", but here times count from the
    /// epoch, so an ejection that ends at the epoch also stores zero.
    has_ejection: bool,
    hc_successes: i64,
    hc_failures: i64,
    consecutive_gw: i64,
    successes: u64,
    failures: u64,
    times_ejected: i64,
    decay_from: i64,
}

impl Model {
    fn record_probe(&mut self, ok: bool, healthy_after: i64, unhealthy_after: i64) -> (bool, bool) {
        let was = self.healthy;
        if ok {
            self.hc_failures = 0;
            self.hc_successes += 1;
            if !was && self.hc_successes >= healthy_after {
                self.healthy = true;
            }
        } else {
            self.hc_successes = 0;
            self.hc_failures += 1;
            if was && self.hc_failures >= unhealthy_after {
                self.healthy = false;
            }
        }
        (self.healthy, self.healthy != was)
    }

    fn record_result(&mut self, gw: bool, failure: bool) -> i64 {
        if gw {
            self.consecutive_gw += 1;
        } else {
            self.consecutive_gw = 0;
        }
        if failure || gw {
            self.failures += 1;
        } else {
            self.successes += 1;
        }
        self.consecutive_gw
    }

    fn eject(&mut self, now: i64, base: i64, max_time: i64) -> i64 {
        self.times_ejected += 1;
        let d = self.times_ejected.min(max_time / base + 1) * base;
        let d = d.min(max_time);
        let until = now + d;
        self.ejected_until = until;
        self.has_ejection = true;
        self.consecutive_gw = 0;
        self.decay_from = until;
        until
    }

    fn ejected(&self, now: i64) -> bool {
        now < self.ejected_until
    }

    fn decay(&mut self, now: i64, period: i64) {
        if self.times_ejected == 0 || self.ejected(now) {
            return;
        }
        while self.times_ejected > 0 && now - self.decay_from >= period {
            self.times_ejected -= 1;
            self.decay_from += period;
        }
    }
}

#[derive(Debug, Clone)]
enum Op {
    Wait(u32),
    Probe(bool, u8, u8),
    Result(bool, bool),
    TakeInterval,
    Eject(u16, u16),
    Decay(u16),
    Clear,
    SetHealthy(bool),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0u32..30_000).prop_map(Op::Wait),
        3 => (any::<bool>(), 1u8..4, 1u8..4).prop_map(|(a, b, c)| Op::Probe(a, b, c)),
        4 => (any::<bool>(), any::<bool>()).prop_map(|(a, b)| Op::Result(a, b)),
        1 => Just(Op::TakeInterval),
        2 => (1u16..2_000, 0u16..10_000).prop_map(|(a, b)| Op::Eject(a, b)),
        2 => (1u16..5_000).prop_map(Op::Decay),
        1 => Just(Op::Clear),
        1 => any::<bool>().prop_map(Op::SetHealthy),
    ]
}

const MS: i64 = 1_000_000;

fn at(epoch: Instant, nanos: i64) -> Instant {
    epoch + Duration::from_nanos(u64::try_from(nanos).unwrap())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn matches_steers_state_machine(ops in prop::collection::vec(op(), 1..120)) {
        let epoch = Instant::now();
        let s = State::new(epoch);
        let mut m = Model { healthy: true, ..Model::default() };
        let mut now = 0i64;
        for op in ops {
            let t = at(epoch, now);
            match op {
                Op::Wait(ms) => now += i64::from(ms) * MS / 10,
                Op::Probe(ok, h, u) => {
                    let got = s.record_probe(ok, "e", u32::from(h), u32::from(u));
                    prop_assert_eq!(got, m.record_probe(ok, i64::from(h), i64::from(u)));
                }
                Op::Result(gw, fail) => {
                    let got = s.record_result(gw, fail);
                    prop_assert_eq!(i64::from(got), m.record_result(gw, fail));
                }
                Op::TakeInterval => {
                    prop_assert_eq!(s.take_interval(), (m.successes, m.failures));
                    m.successes = 0;
                    m.failures = 0;
                }
                Op::Eject(base, max) => {
                    let (base, max) = (i64::from(base) * MS, i64::from(max) * MS);
                    let got = s.eject(t, Duration::from_nanos(base as u64), Duration::from_nanos(max as u64), "r");
                    prop_assert_eq!(got, at(epoch, m.eject(now, base, max)));
                }
                Op::Decay(period) => {
                    let period = i64::from(period) * MS;
                    s.decay(t, Duration::from_nanos(period as u64));
                    m.decay(now, period);
                }
                Op::Clear => {
                    s.clear_ejection();
                    m.ejected_until = 0;
                    m.has_ejection = false;
                    m.times_ejected = 0;
                    m.consecutive_gw = 0;
                }
                Op::SetHealthy(v) => {
                    s.set_healthy(v);
                    m.healthy = v;
                    m.hc_successes = 0;
                    m.hc_failures = 0;
                }
            }
            let t = at(epoch, now);
            prop_assert_eq!(s.healthy(), m.healthy);
            prop_assert_eq!(s.ejected(t), m.ejected(now));
            prop_assert_eq!(s.available(t), m.healthy && !m.ejected(now));
            prop_assert_eq!(s.times_ejected(), u64::try_from(m.times_ejected).unwrap());
            let until = m.has_ejection.then(|| at(epoch, m.ejected_until));
            prop_assert_eq!(s.ejected_until(), until);
            let snap = s.snapshot(t);
            prop_assert_eq!(i64::from(snap.consecutive_gateway_failures), m.consecutive_gw);
            prop_assert_eq!(snap.available, s.available(t));
        }
    }
}
