//! Random operation scripts for the pacer differential test.
//!
//! A script is a list of diffdrive lines (see `legacy/pacer/tests/diffdrive.c`)
//! generated from one seed. It works a few GCRA limiters, keyed tables and
//! sliding windows at once under one moving clock, and aims at the edges
//! where ports go wrong:
//!
//! - rates from one token per tick (an interval of 1) to one per tens of
//!   seconds, bursts from 1 to a million, and invalid rates (a zero field
//!   or a tolerance past 2^62);
//! - costs of 0, of exactly the burst, just past it and of `u64::MAX`;
//!   ranges with `min` above `max` and above the burst; refunds past a
//!   full bucket;
//! - tables of one to a few hundred keys fed from a pool of 40, so shards
//!   fill up and the deny and evict policies, idle reclaim, hold caps and
//!   backward-shift deletion all run; invalid configurations and keys that
//!   are empty or longer than 32 bytes;
//! - windows of one to sixteen buckets, counts that wrap at 2^64, and a
//!   clock that jumps forward by up to a day, past the limiter's 2^57 ns
//!   clamp, and sometimes back.
//!
//! The script ends with a dump of every live object: each limiter's state,
//! each window's count and each table's counters, invariant check and the
//! state of every key in the pool, so the final contents are compared even
//! where no random operation looked.
//!
//! Generation needs no answers from either implementation: whether an
//! object exists after an `init` or `create` is decided here from the
//! documented rules, and a line on an object the C driver does not have
//! would make it exit, which the comparison reports at that step.

use std::fmt::Write as _;

use edgefront_limiter::Rate;
use edgefront_limiter::gcra::MAX_TOL_TICKS;
use edgefront_limiter::table::{KEY_MAX, MAX_ENTRIES, MAX_SHARDS};
use edgefront_limiter::window::SLOTS_MAX;

use super::hex_key;
use crate::rng::Rng;

/// The generated clock never passes this, which keeps libpacer's window
/// arithmetic (`head_start + slot_ns`) from wrapping; the documented
/// difference between the two windows is only reachable above it.
pub const CLOCK_MAX: u64 = 1 << 60;

const GCRAS: usize = 4;
const TABLES: usize = 3;
const WINDOWS: usize = 3;
const POOL: usize = 40;
/// The pool's first keys take half of all keyed operations.
const HOT: usize = 8;
/// The hash key libpacer's own table tests use.
const FIXED_HASH_KEY: &str = "0102030405060708090a0b0c0d0e0f10";

/// A generated script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    pub seed: u64,
    /// One operation per line, each printing exactly one line.
    pub lines: Vec<String>,
}

impl Script {
    /// `ops` random operations from `seed`, followed by the state dump.
    /// An odd seed's script first sets the clock, an extra line.
    /// Step `k` depends only on the seed and the steps before it, so a
    /// longer script from the same seed starts with the shorter one.
    pub fn generate(seed: u64, ops: usize) -> Script {
        let mut g = Gen::new(seed);
        for _ in 0..ops {
            g.step();
        }
        g.dump();
        Script {
            seed,
            lines: g.lines,
        }
    }

    /// The script as the driver reads it.
    pub fn text(&self) -> String {
        let mut s = String::with_capacity(self.lines.len() * 24);
        for line in &self.lines {
            s.push_str(line);
            s.push('\n');
        }
        s
    }
}

/// Whether libpacer's tolerance product `interval * burst` wraps its 128
/// bits for this rate, where edgefront-limiter rejects it instead.
pub fn wraps_in_c(rate: &Rate) -> bool {
    if rate.tokens == 0 || rate.period_ns == 0 {
        return false;
    }
    let iv = (u128::from(rate.period_ns) << 6).div_ceil(u128::from(rate.tokens));
    iv.checked_mul(u128::from(rate.burst)).is_none()
}

/// The largest cost a valid rate admits (its tolerance over its interval),
/// or `None` for a rate both implementations reject.
pub fn capacity(rate: &Rate) -> Option<u64> {
    rate.ticks().ok().map(|(iv, tol)| tol / iv)
}

/// A number whose bit length is uniform in `0..=bits`, so small and large
/// magnitudes are equally likely.
fn wide(rng: &mut Rng, bits: u32) -> u64 {
    match rng.below(bits as usize + 1) as u32 {
        0 => 0,
        b => rng.next_u64() >> (64 - b) | 1 << (b - 1),
    }
}

struct Gen {
    rng: Rng,
    now: u64,
    keys: Vec<Vec<u8>>,
    /// Each live limiter's capacity.
    gcras: [Option<u64>; GCRAS],
    /// Each live table's capacity under its current rate.
    tables: [Option<u64>; TABLES],
    windows: [bool; WINDOWS],
    lines: Vec<String>,
}

impl Gen {
    fn new(seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let keys = (0..POOL)
            .map(|i| {
                // Short keys that differ in one byte, and longer ones up to
                // the limit, so probe chains and hash spread both vary.
                let len = match i % 4 {
                    0 | 1 => rng.range(1, 2),
                    2 => rng.range(3, 12),
                    _ => rng.range(13, KEY_MAX),
                };
                (0..len).map(|_| rng.below(256) as u8).collect()
            })
            .collect();
        // Half the scripts start the clock at 0, so fresh limiters meet a
        // clock below their first interval; the rest start elsewhere.
        let now = if seed.is_multiple_of(2) {
            0
        } else {
            rng.next_u64() % (1 << 40)
        };
        let lines = if now == 0 {
            Vec::new()
        } else {
            vec![format!("now {now}")]
        };
        Self {
            rng,
            now,
            keys,
            gcras: [None; GCRAS],
            tables: [None; TABLES],
            windows: [false; WINDOWS],
            lines,
        }
    }

    fn emit(&mut self, line: String) {
        self.lines.push(line);
    }

    fn step(&mut self) {
        match self.rng.below(100) {
            0..=11 => self.clock(),
            12 => self.emit("sync".into()),
            13..=37 => self.gcra(),
            38..=84 => self.table(),
            _ => self.window(),
        }
    }

    fn dump(&mut self) {
        for i in 0..GCRAS {
            if self.gcras[i].is_some() {
                self.emit(format!("gcra {i} state"));
            }
        }
        for i in 0..WINDOWS {
            if self.windows[i] {
                self.emit(format!("window {i} count"));
            }
        }
        for i in 0..TABLES {
            if self.tables[i].is_some() {
                self.emit(format!("table {i} stats"));
                self.emit(format!("table {i} check"));
                for k in 0..POOL {
                    let key = hex_key(&self.keys[k]);
                    self.emit(format!("table {i} get {key}"));
                }
            }
        }
    }

    fn clock(&mut self) {
        if self.rng.chance(0.1) {
            // Anywhere, often backwards, sometimes past the 2^57 clamp.
            self.now = if self.rng.chance(0.5) {
                self.rng.next_u64() % (self.now + 1)
            } else {
                wide(&mut self.rng, 60)
            };
            let line = format!("now {}", self.now);
            return self.emit(line);
        }
        let by = match self.rng.below(20) {
            0..=5 => self.rng.below(1001) as u64,
            6..=13 => wide(&mut self.rng, 30),
            14..=18 => wide(&mut self.rng, 36),
            _ => wide(&mut self.rng, 47),
        };
        let by = by.min(CLOCK_MAX - self.now);
        self.now += by;
        self.emit(format!("advance {by}"));
    }

    /// A rate, now and then an invalid one. Never one whose tolerance
    /// product wraps in libpacer.
    fn rate(&mut self) -> Rate {
        let r = &mut self.rng;
        if r.chance(0.04) {
            return match r.below(4) {
                0 => Rate::new(0, 1_000_000_000, 1),
                1 => Rate::new(10, 0, 1),
                2 => Rate::new(10, 1_000_000_000, 0),
                // Tolerance past 2^62 ticks: 2^62 / 64 ns of burst at one
                // token per ns, plus one.
                _ => Rate::new(1, 1, (MAX_TOL_TICKS >> 6) + 1 + wide(r, 20)),
            };
        }
        let tokens = 1 + wide(r, 30);
        let period_ns = 1 + wide(r, 34);
        let burst = match r.below(10) {
            0..=5 => r.range(1, 16) as u64,
            6..=8 => r.range(1, 1000) as u64,
            _ => 1 + wide(r, 20),
        };
        let rate = Rate::new(tokens, period_ns, burst);
        if wraps_in_c(&rate) {
            Rate::new(tokens, period_ns, 1)
        } else {
            rate
        }
    }

    /// An amount near a capacity: 0, small, exactly it, just past it, or
    /// anything up to `u64::MAX`.
    fn amount(&mut self, cap: u64) -> u64 {
        let r = &mut self.rng;
        match r.below(20) {
            0..=12 => r.next_u64() % (cap.saturating_add(1)),
            13..=15 => r.range(0, 3) as u64,
            16 => cap,
            17 => cap.saturating_add(r.range(1, 3) as u64),
            18 => u64::MAX,
            _ => wide(r, 64),
        }
    }

    fn rate_words(rate: &Rate) -> String {
        format!("{} {} {}", rate.tokens, rate.period_ns, rate.burst)
    }

    fn gcra(&mut self) {
        let i = self.rng.below(GCRAS);
        let Some(cap) = self.gcras[i] else {
            return self.gcra_init(i);
        };
        let line = match self.rng.below(100) {
            0..=29 => format!("gcra {i} take {}", self.amount(cap)),
            30..=44 => format!("gcra {i} take_upto {}", self.amount(cap.saturating_mul(2))),
            45..=59 => {
                let min = self.amount(cap);
                let max = if self.rng.chance(0.8) {
                    min.saturating_add(self.amount(cap))
                } else {
                    self.amount(cap)
                };
                format!("gcra {i} take_range {min} {max}")
            }
            60..=71 => format!("gcra {i} refund {}", self.amount(cap)),
            72..=81 => format!("gcra {i} remaining"),
            82..=89 => {
                let rate = self.rate();
                if let Some(cap) = capacity(&rate) {
                    self.gcras[i] = Some(cap);
                }
                format!("gcra {i} reconfigure {}", Self::rate_words(&rate))
            }
            90..=94 => format!("gcra {i} state"),
            _ => return self.gcra_init(i),
        };
        self.emit(line);
    }

    fn gcra_init(&mut self, i: usize) {
        let rate = self.rate();
        if let Some(cap) = capacity(&rate) {
            self.gcras[i] = Some(cap);
        }
        self.emit(format!("gcra {i} init {}", Self::rate_words(&rate)));
    }

    /// A key from the pool, the hot end half the time, and now and then
    /// one the table must refuse.
    fn key(&mut self) -> String {
        let r = &mut self.rng;
        if r.chance(0.03) {
            let len = if r.chance(0.5) {
                0
            } else {
                r.range(KEY_MAX + 1, 64)
            };
            return hex_key(&vec![b'k'; len]);
        }
        let k = if r.chance(0.5) {
            r.below(HOT)
        } else {
            r.below(POOL)
        };
        hex_key(&self.keys[k])
    }

    fn table(&mut self) {
        let i = self.rng.below(TABLES);
        let Some(cap) = self.tables[i] else {
            return self.table_create(i);
        };
        let key = self.key();
        let line = match self.rng.below(100) {
            0..=24 => format!("table {i} take {key} {}", self.amount(cap)),
            25..=34 => format!("table {i} take_upto {key} {}", self.amount(cap)),
            35..=42 => {
                let (min, max) = (self.amount(cap), self.amount(cap.saturating_mul(2)));
                format!("table {i} take_range {key} {min} {max}")
            }
            43..=50 => format!("table {i} refund {key} {}", self.amount(cap)),
            51..=60 => format!("table {i} hold {key} {}", self.rng.below(4)),
            61..=68 => format!("table {i} release {key}"),
            69..=73 => format!("table {i} remove {key}"),
            74..=83 => format!("table {i} get {key}"),
            84..=86 => {
                let rate = self.rate();
                if let Some(cap) = capacity(&rate) {
                    self.tables[i] = Some(cap);
                }
                format!("table {i} reconfigure {}", Self::rate_words(&rate))
            }
            87..=91 => {
                let n = match self.rng.below(4) {
                    0 => 0,
                    1 | 2 => self.rng.range(1, 4) as u32,
                    _ => self.rng.pick(&[64, 1024, u32::MAX]),
                };
                format!("table {i} sweep {n}")
            }
            92..=95 => format!("table {i} stats"),
            96..=98 => format!("table {i} check"),
            _ => {
                self.tables[i] = None;
                format!("table {i} destroy")
            }
        };
        self.emit(line);
    }

    fn table_create(&mut self, i: usize) {
        let r = &mut self.rng;
        let max = match r.below(100) {
            0..=69 => r.range(1, 16) as u32,
            70..=95 => r.range(17, 256) as u32,
            96 | 97 => 0,
            _ => MAX_ENTRIES + 1,
        };
        let shards = match r.below(10) {
            0..=3 => 0,
            4..=8 => 1 << r.below(6),
            _ => r.pick(&[3, 6, 12, MAX_SHARDS * 2]),
        };
        let policy = r.pick(&["deny", "evict"]);
        let hash_key = if r.chance(0.5) {
            FIXED_HASH_KEY.to_owned()
        } else {
            (0..16).fold(String::new(), |mut s, _| {
                let _ = write!(s, "{:02x}", r.below(256));
                s
            })
        };
        let rate = self.rate();
        let shards_ok = shards == 0 || (shards.is_power_of_two() && shards <= max);
        if (1..=MAX_ENTRIES).contains(&max) && shards_ok {
            self.tables[i] = capacity(&rate);
        }
        self.emit(format!(
            "table {i} create {max} {shards} {} {policy} {hash_key}",
            Self::rate_words(&rate)
        ));
    }

    fn window(&mut self) {
        let i = self.rng.below(WINDOWS);
        if !self.windows[i] {
            return self.window_init(i);
        }
        let line = match self.rng.below(100) {
            0..=44 => {
                let r = &mut self.rng;
                let n = match r.below(20) {
                    0..=13 => r.below(11) as u64,
                    14..=18 => wide(r, 32),
                    _ => u64::MAX - r.below(4) as u64,
                };
                format!("window {i} add {n}")
            }
            45..=91 => format!("window {i} count"),
            _ => return self.window_init(i),
        };
        self.emit(line);
    }

    fn window_init(&mut self, i: usize) {
        let r = &mut self.rng;
        let slots = match r.below(20) {
            0 => 0,
            1 => r.range(SLOTS_MAX as usize + 1, 20) as u32,
            _ => r.range(1, SLOTS_MAX as usize) as u32,
        };
        let window_ns = wide(r, 36);
        if (1..=SLOTS_MAX).contains(&slots) && window_ns >= u64::from(slots) {
            self.windows[i] = true;
        }
        self.emit(format!("window {i} init {window_ns} {slots}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_are_deterministic_and_prefix_stable() {
        let a = Script::generate(11, 500);
        assert_eq!(a, Script::generate(11, 500));
        assert_ne!(a, Script::generate(12, 500));
        let longer = Script::generate(11, 800);
        // The random steps come first; a longer script extends them.
        let dump_free = |s: &Script, n: usize| s.lines[..n].to_vec();
        assert_eq!(dump_free(&a, 500), dump_free(&longer, 500));
        assert!(a.text().ends_with('\n'));
        assert_eq!(a.text().lines().count(), a.lines.len());
    }

    #[test]
    fn generated_rates_never_wrap_in_c() {
        let mut g = Gen::new(3);
        let mut invalid = 0;
        for _ in 0..20_000 {
            let rate = g.rate();
            assert!(!wraps_in_c(&rate), "{rate:?}");
            invalid += usize::from(capacity(&rate).is_none());
        }
        assert!((400..1200).contains(&invalid), "{invalid}");
        // The guard itself: an interval of 2^70 ticks times a full burst.
        assert!(wraps_in_c(&Rate::new(1, u64::MAX, u64::MAX)));
        assert!(!wraps_in_c(&Rate::new(1, u64::MAX, 1)));
        assert!(!wraps_in_c(&Rate::new(0, 1, u64::MAX)));
        assert!(!wraps_in_c(&Rate::new(1, 0, u64::MAX)));
    }

    #[test]
    fn the_clock_stays_below_its_ceiling() {
        let s = Script::generate(5, 50_000);
        let mut now = 0;
        for line in &s.lines {
            if let Some(v) = line.strip_prefix("now ") {
                now = v.parse().unwrap();
            } else if let Some(v) = line.strip_prefix("advance ") {
                now += v.parse::<u64>().unwrap();
            }
            assert!(now <= CLOCK_MAX, "{line}");
        }
        assert!(s.lines[0].starts_with("now "), "{}", s.lines[0]);
        assert!(!Script::generate(4, 10).lines[0].starts_with("now "));
    }

    #[test]
    fn wide_numbers_spread_over_bit_lengths() {
        let mut r = Rng::new(1);
        let mut lengths = [0usize; 65];
        for _ in 0..65_000 {
            lengths[64 - wide(&mut r, 64).leading_zeros() as usize] += 1;
        }
        assert!(
            lengths.iter().all(|&n| (800..1200).contains(&n)),
            "{lengths:?}"
        );
        assert_eq!(wide(&mut r, 0), 0);
    }
}
