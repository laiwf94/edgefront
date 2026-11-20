//! Bursts with delay, as NGINX's `limit_req` does them: requests over the
//! rate are queued, each held until its turn, instead of refused, up to a
//! burst; with `nodelay` they pass at once, and with `delay=N` the first N
//! over the rate pass and the rest are held.
//!
//! The arithmetic is NGINX's own (`ngx_http_limit_req_module.c`), in its
//! units, so a config moved from NGINX gives the same answers to the
//! millisecond: time in whole milliseconds, the rate in thousandths of a
//! request per second (`10r/s` is 10,000, `30r/m` is 500), and each key's
//! *excess* in thousandths of a request. A request at `now`:
//!
//! 1. `excess = old_excess - rate * (now - last) / 1000 + 1000`, at least 0
//!    (a clock that went back by under a minute counts as no time; by more,
//!    as one millisecond);
//! 2. refused when `excess > burst * 1000`, and nothing changes;
//! 3. otherwise the key keeps `excess`, and `last` moves to `now` unless no
//!    time passed (NGINX's `if (ms)`), and the request passes after
//!    `(excess - delay * 1000) * 1000 / rate` ms, or at once when that is
//!    not positive. `delay` is 0 by default (every request over the rate
//!    waits), N for `delay=N`, and unbounded for `nodelay`.
//!
//! A caller holding a request for its delay is queueing it up to a
//! deadline: the burst bounds the queue, since a request that would wait
//! longer than `burst / rate` is refused instead. Keys live in a
//! [`LimitReqTable`] of fixed capacity, as NGINX's zone does; a new key in
//! a full table first replaces one whose excess has drained to zero, and
//! is refused if none has, as NGINX answers when its zone is full.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

/// When requests over the rate pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delay {
    /// Every request over the rate waits for its turn (the default).
    All,
    /// The first N over the rate pass at once (`delay=N`).
    After(u64),
    /// None waits (`nodelay`).
    NoDelay,
}

/// A `limit_req` zone's rate and a location's burst and delay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitReq {
    /// Thousandths of a request per second.
    pub rate_milli: u64,
    pub burst: u64,
    pub delay: Delay,
}

/// What happens to a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Pass after this many milliseconds (0: at once).
    Pass { delay_ms: u64 },
    /// Over the burst: refused (NGINX's 503, or `limit_req_status`).
    Refuse,
}

/// One key's state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Excess {
    pub excess: u64,
    pub last_ms: u64,
}

impl LimitReq {
    /// Reads NGINX's `rate=` value, `10r/s` or `30r/m`.
    pub fn parse_rate(text: &str) -> Result<u64, String> {
        let bad = || format!("{text:?} is not a rate like 10r/s or 30r/m");
        let (n, per) = text.split_once("r/").ok_or_else(bad)?;
        let n: u64 = n.parse().map_err(|_| bad())?;
        let milli = match per {
            "s" => n.checked_mul(1000),
            "m" => n.checked_mul(1000).map(|v| v / 60),
            _ => None,
        };
        milli.filter(|&m| m > 0).ok_or_else(bad)
    }

    /// A key's first request: NGINX starts it with no excess and lets it
    /// pass.
    pub fn first(now_ms: u64) -> (Excess, Outcome) {
        (
            Excess {
                excess: 0,
                last_ms: now_ms,
            },
            Outcome::Pass { delay_ms: 0 },
        )
    }

    /// The decision for a known key with state `st` at `now_ms`, updating
    /// `st` unless the request is refused.
    pub fn decide(&self, st: &mut Excess, now_ms: u64) -> Outcome {
        let ms: i64 = if now_ms >= st.last_ms {
            i64::try_from(now_ms - st.last_ms).unwrap_or(i64::MAX)
        } else if st.last_ms - now_ms > 60_000 {
            1
        } else {
            0
        };
        let drained = i128::from(self.rate_milli) * i128::from(ms) / 1000;
        let excess = (i128::from(st.excess) - drained + 1000).max(0);
        if excess > i128::from(self.burst) * 1000 {
            return Outcome::Refuse;
        }
        st.excess = excess as u64;
        if ms != 0 {
            st.last_ms = now_ms;
        }
        let free = match self.delay {
            Delay::All => 0,
            Delay::After(n) => i128::from(n) * 1000,
            Delay::NoDelay => return Outcome::Pass { delay_ms: 0 },
        };
        if excess <= free {
            return Outcome::Pass { delay_ms: 0 };
        }
        let delay = (excess - free) * 1000 / i128::from(self.rate_milli);
        Outcome::Pass {
            delay_ms: u64::try_from(delay).unwrap_or(u64::MAX),
        }
    }
}

/// Keys' excess, sharded, at most `capacity` keys.
pub struct LimitReqTable {
    limit: LimitReq,
    shards: Vec<Mutex<HashMap<Vec<u8>, Excess>>>,
    per_shard: usize,
}

impl std::fmt::Debug for LimitReqTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LimitReqTable")
            .field("limit", &self.limit)
            .finish()
    }
}

impl LimitReqTable {
    pub fn new(limit: LimitReq, capacity: usize) -> Self {
        let n = 16;
        LimitReqTable {
            limit,
            shards: (0..n).map(|_| Mutex::new(HashMap::new())).collect(),
            per_shard: capacity.div_ceil(n).max(1),
        }
    }

    /// The decision for `key` at `now_ms`. A new key finding its shard
    /// full is refused unless a key there has drained.
    pub fn decide(&self, key: &[u8], now_ms: u64) -> Outcome {
        use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
        let h = BuildHasherDefault::<DefaultHasher>::default().hash_one(key) as usize;
        let mut s = self.shards[h % self.shards.len()]
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(st) = s.get_mut(key) {
            return self.limit.decide(st, now_ms);
        }
        if s.len() >= self.per_shard {
            let rate = self.limit.rate_milli;
            let drained = s
                .iter()
                .find(|(_, e)| {
                    e.excess <= rate.saturating_mul(now_ms.saturating_sub(e.last_ms)) / 1000
                })
                .map(|(k, _)| k.clone());
            match drained {
                Some(k) => {
                    s.remove(&k);
                }
                None => return Outcome::Refuse,
            }
        }
        let (st, out) = LimitReq::first(now_ms);
        s.insert(key.to_vec(), st);
        out
    }

    /// Keys held.
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().unwrap_or_else(PoisonError::into_inner).len())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lr(rate: &str, burst: u64, delay: Delay) -> LimitReq {
        LimitReq {
            rate_milli: LimitReq::parse_rate(rate).unwrap(),
            burst,
            delay,
        }
    }

    fn run(l: &LimitReq, times: &[u64]) -> Vec<Outcome> {
        let (mut st, first) = LimitReq::first(times[0]);
        let mut out = vec![first];
        out.extend(times[1..].iter().map(|&t| l.decide(&mut st, t)));
        out
    }

    fn pass(ms: u64) -> Outcome {
        Outcome::Pass { delay_ms: ms }
    }

    #[test]
    fn rates_parse_in_nginx_units() {
        assert_eq!(LimitReq::parse_rate("10r/s").unwrap(), 10_000);
        assert_eq!(LimitReq::parse_rate("30r/m").unwrap(), 500);
        assert_eq!(LimitReq::parse_rate("1r/m").unwrap(), 16);
        for bad in ["10/s", "r/s", "10r/h", "0r/s", "xr/s"] {
            assert!(LimitReq::parse_rate(bad).is_err(), "{bad}");
        }
    }

    /// NGINX's documented example: `rate=1r/s burst=5`, six requests at
    /// once: the first passes, five wait 1 to 5 s, a seventh is refused.
    #[test]
    fn a_burst_is_queued_one_rate_step_apart() {
        let l = lr("1r/s", 5, Delay::All);
        let got = run(&l, &[0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            got,
            [
                pass(0),
                pass(1000),
                pass(2000),
                pass(3000),
                pass(4000),
                pass(5000),
                Outcome::Refuse
            ]
        );
        // nodelay: all six pass at once; the seventh is still refused.
        let l = lr("1r/s", 5, Delay::NoDelay);
        assert_eq!(run(&l, &[0; 7])[..6], [pass(0); 6]);
        assert_eq!(run(&l, &[0; 7])[6], Outcome::Refuse);
        // delay=2: two over the rate pass at once, then the wait grows.
        let l = lr("1r/s", 5, Delay::After(2));
        assert_eq!(
            run(&l, &[0; 7]),
            [
                pass(0),
                pass(0),
                pass(0),
                pass(1000),
                pass(2000),
                pass(3000),
                Outcome::Refuse
            ]
        );
    }

    #[test]
    fn the_queue_drains_at_the_rate() {
        let l = lr("10r/s", 2, Delay::All);
        // 100 ms apart is the rate: never any excess past the first.
        assert_eq!(run(&l, &[0, 100, 200, 300]), [pass(0); 4]);
        // Three at once, then one 150 ms later: the excess (2000) has
        // drained by 1500 to 500, plus 1000 for the new one: 150 ms.
        assert_eq!(
            run(&l, &[0, 0, 0, 150]),
            [pass(0), pass(100), pass(200), pass(150)]
        );
        // 30r/m: the rate in thousandths floors to 500.
        let l = lr("30r/m", 1, Delay::All);
        assert_eq!(
            run(&l, &[0, 0, 0, 2000]),
            [pass(0), pass(2000), Outcome::Refuse, pass(2000)]
        );
    }

    #[test]
    fn clocks_going_back_count_as_nginx_counts_them() {
        let l = lr("1r/s", 10, Delay::NoDelay);
        let mut st = Excess {
            excess: 5000,
            last_ms: 100_000,
        };
        // Back 10 s: no time passed, last stays.
        assert_eq!(l.decide(&mut st, 90_000), pass(0));
        assert_eq!(
            st,
            Excess {
                excess: 6000,
                last_ms: 100_000
            }
        );
        // Back over a minute: one millisecond.
        l.decide(&mut st, 10_000);
        assert_eq!(
            st,
            Excess {
                excess: 6999,
                last_ms: 10_000
            }
        );
        // A refusal changes nothing.
        let mut full = Excess {
            excess: 10_000,
            last_ms: 0,
        };
        assert_eq!(l.decide(&mut full, 0), Outcome::Refuse);
        assert_eq!(
            full,
            Excess {
                excess: 10_000,
                last_ms: 0
            }
        );
    }

    #[test]
    fn a_full_table_replaces_drained_keys_only() {
        let t = LimitReqTable::new(lr("1r/s", 0, Delay::All), 16);
        assert!(t.is_empty());
        let now = 1_000_000;
        // A key's first request leaves it no excess, so it is drained at
        // once and a newcomer may replace it.
        for i in 0..200u32 {
            assert_eq!(t.decide(&i.to_be_bytes(), now), pass(0));
        }
        assert!(t.len() <= 16);
        // Keys that have not drained hold their place.
        let t = LimitReqTable::new(lr("1r/s", 5, Delay::All), 16);
        for i in 0..16u32 {
            for _ in 0..3 {
                t.decide(&i.to_be_bytes(), now);
            }
        }
        let refused = (16..64u32)
            .filter(|i| t.decide(&i.to_be_bytes(), now) == Outcome::Refuse)
            .count();
        assert!(refused > 0, "some shard is full of keys in debt");
        assert!(format!("{t:?}").contains("LimitReqTable"));
    }
}
