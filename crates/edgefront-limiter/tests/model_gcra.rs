//! Model check for the GCRA limiter, replacing libpacer's
//! tests/model_gcra.c. Random operation sequences drive `Gcra::take`,
//! `take_upto`, `take_range`, `refund`, `remaining` and `reconfigure` and
//! the same operations on a slow reference: a plain token bucket whose
//! level is an exact fraction, refilled continuously and capped at the
//! burst. The two must agree on every result and every decision field.
//!
//! The reference knows nothing about arrival times or ticks. What it shares
//! with the library is the documented contract only:
//!   - the refill rate is tokens/period with the time per token rounded up
//!     to the next 1/64 ns (checked here to be never faster than configured
//!     and within 1/interval of it);
//!   - reconfigure keeps debt as time, so the level becomes
//!     burst' - (burst - level) * rate' / rate, which can be negative;
//!   - retry_after_ns is the exact wait rounded up to a whole nanosecond.
//!
//! Scripts come from two sources that share one generator, ported from the
//! C: a splitmix64 stream per seed, and a proptest-generated tape of random
//! words that proptest shrinks on failure. Every divergence is reported
//! with its seed (or the shrunk tape), step and the operations leading to
//! it. Rerun one seed with `EDGEFRONT_MODEL_SEED=<seed>`, or widen the
//! search with `EDGEFRONT_MODEL_SEEDS=<count>`:
//!
//! ```text
//! EDGEFRONT_MODEL_SEED=17 cargo test -p edgefront-limiter --test model_gcra
//! ```

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::fmt;

use edgefront_limiter::gcra::MAX_TOL_TICKS;
use edgefront_limiter::{Decision, Error, Gcra, Rate};
use proptest::prelude::*;

const DEFAULT_SEEDS: u64 = 300;
const STEPS: usize = 2000;
const HISTORY: usize = 8;
/// Well inside the clamp at 2^57.
const HORIZON_NS: u64 = 1 << 52;

// ---- exact rationals: num/den, den > 0, always reduced ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rat {
    num: i128,
    den: i128,
}

fn overflow() -> ! {
    panic!("model_gcra: rational overflow; the generator produced values too large")
}

fn mul(a: i128, b: i128) -> i128 {
    a.checked_mul(b).unwrap_or_else(|| overflow())
}

fn add(a: i128, b: i128) -> i128 {
    a.checked_add(b).unwrap_or_else(|| overflow())
}

fn gcd(a: i128, b: i128) -> i128 {
    let (mut a, mut b) = (a.abs(), b.abs());
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

impl Rat {
    fn new(num: i128, den: i128) -> Self {
        assert!(den != 0, "model_gcra: division by zero in the reference");
        let (num, den) = if den < 0 { (-num, -den) } else { (num, den) };
        let g = gcd(num, den);
        if g > 1 {
            Rat {
                num: num / g,
                den: den / g,
            }
        } else {
            Rat { num, den }
        }
    }

    fn int(v: u64) -> Self {
        Rat {
            num: i128::from(v),
            den: 1,
        }
    }

    fn add(self, b: Rat) -> Rat {
        let g = gcd(self.den, b.den);
        let num = add(mul(self.num, b.den / g), mul(b.num, self.den / g));
        Rat::new(num, mul(self.den / g, b.den))
    }

    fn neg(self) -> Rat {
        Rat {
            num: -self.num,
            den: self.den,
        }
    }

    fn sub(self, b: Rat) -> Rat {
        self.add(b.neg())
    }

    fn mul(self, b: Rat) -> Rat {
        // Cross-reduce first so the products stay small.
        let g1 = gcd(self.num, b.den).max(1);
        let g2 = gcd(b.num, self.den).max(1);
        Rat::new(
            mul(self.num / g1, b.num / g2),
            mul(self.den / g2, b.den / g1),
        )
    }

    fn div(self, b: Rat) -> Rat {
        self.mul(Rat::new(b.den, b.num))
    }

    fn cmp(self, b: Rat) -> Ordering {
        mul(self.num, b.den).cmp(&mul(b.num, self.den))
    }

    fn min(self, b: Rat) -> Rat {
        if self.cmp(b) != Ordering::Greater {
            self
        } else {
            b
        }
    }

    /// floor(self) for self >= 0.
    fn floor_nonneg(self) -> u64 {
        u64::try_from(self.num / self.den).expect("floor fits in u64")
    }

    /// ceil(self) for self >= 0.
    fn ceil_nonneg(self) -> u64 {
        u64::try_from((self.num + self.den - 1) / self.den).expect("ceil fits in u64")
    }
}

// ---- the reference token bucket ----

#[derive(Clone, Debug)]
struct RefBucket {
    burst: u64,
    /// Tokens per ns, after the documented rounding.
    rate: Rat,
    level: Rat,
    /// ns of the last operation.
    now: u64,
}

/// The documented rate rule: time per token is period/tokens ns rounded up
/// to the next 1/64 ns. None when the library must refuse the rate.
fn ref_rate(r: &Rate) -> Option<Rat> {
    if r.tokens == 0 || r.period_ns == 0 || r.burst == 0 {
        return None;
    }
    let per_token = Rat::new(i128::from(r.period_ns), i128::from(r.tokens));
    let ticks = per_token.mul(Rat::int(64));
    let iv = (ticks.num + ticks.den - 1) / ticks.den;
    // The tolerance, burst * per-token time, is limited to 2^62 ticks.
    if mul(iv, i128::from(r.burst)) > i128::from(MAX_TOL_TICKS) {
        return None;
    }
    Some(Rat::new(64, iv))
}

/// The rounded rate is never faster than configured and is slower by less
/// than one part in the interval.
fn rate_within_contract(r: &Rate, rate: Rat) -> bool {
    let exact = Rat::new(i128::from(r.tokens), i128::from(r.period_ns));
    let iv = Rat::int(64).div(rate);
    let shortfall = exact.sub(rate).div(exact);
    rate.cmp(exact) != Ordering::Greater && shortfall.cmp(Rat::int(1).div(iv)) == Ordering::Less
}

impl RefBucket {
    fn new(r: &Rate, now: u64) -> Option<Self> {
        Some(RefBucket {
            burst: r.burst,
            rate: ref_rate(r)?,
            level: Rat::int(r.burst),
            now,
        })
    }

    fn advance(&mut self, now: u64) {
        let refill = Rat::int(now - self.now).mul(self.rate);
        self.level = Rat::int(self.burst).min(self.level.add(refill));
        self.now = now;
    }

    fn whole(&self) -> u64 {
        if self.level.num <= 0 {
            0
        } else {
            self.level.floor_nonneg()
        }
    }

    /// Nanoseconds, rounded up, until the level reaches `want`.
    fn wait(&self, want: u64) -> u64 {
        Rat::int(want).sub(self.level).div(self.rate).ceil_nonneg()
    }

    fn take(&mut self, now: u64, cost: u64) -> Result<Decision, Error> {
        self.advance(now);
        if cost > self.burst {
            return Err(Error::CostExceedsBurst);
        }
        if self.level.cmp(Rat::int(cost)) == Ordering::Less {
            return Ok(Decision {
                allowed: false,
                granted: 0,
                remaining: self.whole(),
                retry_after_ns: self.wait(cost),
            });
        }
        self.level = self.level.sub(Rat::int(cost));
        Ok(Decision {
            allowed: true,
            granted: cost,
            remaining: self.whole(),
            retry_after_ns: 0,
        })
    }

    fn take_range(&mut self, now: u64, min: u64, max: u64) -> Decision {
        self.advance(now);
        let avail = self.whole();
        let min = min.clamp(1, self.burst);
        let mut d = Decision {
            allowed: false,
            granted: 0,
            remaining: avail,
            retry_after_ns: 0,
        };
        if max == 0 {
            return d;
        }
        if avail < min {
            d.retry_after_ns = self.wait(min);
            return d;
        }
        let grant = avail.min(max);
        self.level = self.level.sub(Rat::int(grant));
        Decision {
            allowed: true,
            granted: grant,
            remaining: avail - grant,
            retry_after_ns: 0,
        }
    }

    fn refund(&mut self, now: u64, tokens: u64) {
        self.advance(now);
        self.level = Rat::int(self.burst).min(self.level.add(Rat::int(tokens)));
    }

    fn remaining(&mut self, now: u64) -> u64 {
        self.advance(now);
        self.whole()
    }

    fn reconfigure(&mut self, r: &Rate) -> Result<(), Error> {
        let rate = ref_rate(r).ok_or(Error::InvalidRate)?;
        // Debt in ns stays the same; the level is what that debt leaves of
        // the new burst at the new rate.
        let debt_ns = Rat::int(self.burst).sub(self.level).div(self.rate);
        self.level = Rat::int(r.burst).sub(debt_ns.mul(rate));
        self.burst = r.burst;
        self.rate = rate;
        Ok(())
    }
}

// ---- random generation, ported from model_gcra.c ----

/// A stream of random words: splitmix64 for seeded runs, or a tape of
/// words chosen (and shrunk) by proptest, read as zeros once used up.
trait Source {
    fn next(&mut self) -> u64;

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    /// Roughly log-uniform in [1, 10^digits].
    fn log(&mut self, digits: u32) -> u64 {
        let scale = 10u64.pow(self.below(u64::from(digits) + 1) as u32);
        1 + self.below(scale)
    }
}

struct SplitMix(u64);

impl Source for SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

struct Tape<'a> {
    words: &'a [u64],
    at: usize,
}

impl Source for Tape<'_> {
    fn next(&mut self) -> u64 {
        let w = self.words.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        w
    }
}

fn gen_rate(s: &mut impl Source) -> Rate {
    let mut r = Rate::new(s.log(9), s.log(11), 0);
    r.burst = match s.below(10) {
        0 => 1,
        1 => s.log(6),
        _ => 1 + s.below(40),
    };
    // Now and then an invalid rate, which must be refused by both.
    match s.below(40) {
        0 => r.tokens = 0,
        1 => r.burst = 1 << 62,
        2 => r.period_ns = 0,
        _ => {}
    }
    r
}

/// A time step that keeps the bucket moving between empty and full.
fn gen_step(s: &mut impl Source, m: &RefBucket, steps_left: usize) -> u64 {
    let token_ns = Rat::int(1).div(m.rate).ceil_nonneg();
    let dt = match s.below(8) {
        // Several operations at the same instant.
        0 | 1 => 0,
        // Sub-token steps exercise rounding.
        2 => 1 + s.below(3),
        3..=5 => s.below(token_ns + 1),
        6 => s.below(token_ns * 4 + 1),
        _ => s.below(token_ns * m.burst.min(64) * 2 + 1),
    };
    let room = (HORIZON_NS - m.now) / (steps_left as u64 + 1);
    dt.min(room)
}

// ---- the driver ----

/// What both sides returned for one operation. Operations without a
/// decision of their own (refund, remaining, reconfigure) report the
/// tokens remaining afterwards.
type Outcome = Result<Decision, Error>;

fn only_remaining(remaining: u64) -> Decision {
    Decision {
        remaining,
        ..Decision::default()
    }
}

struct Divergence {
    step: usize,
    history: Vec<String>,
    want: Outcome,
    got: Outcome,
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "divergence at step {}:", self.step)?;
        for (i, op) in self.history.iter().enumerate() {
            let mark = if i + 1 == self.history.len() {
                ">"
            } else {
                " "
            };
            writeln!(f, "  {mark} {op}")?;
        }
        writeln!(f, "  reference: {:?}", self.want)?;
        write!(f, "  library:   {:?}", self.got)
    }
}

/// Runs one script drawn from `s`. Ok when the library matched the model
/// throughout. The search for a valid starting rate gives up after 16
/// draws and uses one token per nanosecond, so a tape of zeros still makes
/// a script.
fn run(s: &mut impl Source, steps: usize) -> Result<(), String> {
    let mut r = gen_rate(s);
    let mut tries = 0;
    while ref_rate(&r).is_none() {
        tries += 1;
        r = if tries < 16 {
            gen_rate(s)
        } else {
            Rate::new(1, 1, 1)
        };
    }
    let mut m = RefBucket::new(&r, 1 + s.below(1_000_000_000)).expect("valid rate");
    let g = match Gcra::new(&r) {
        Ok(g) if rate_within_contract(&r, m.rate) => g,
        _ => {
            return Err(format!(
                "rate {}/{}ns b{} refused or outside the rounding contract",
                r.tokens, r.period_ns, r.burst
            ));
        }
    };
    let mut history = VecDeque::with_capacity(HISTORY);
    let push = |history: &mut VecDeque<String>, op: String| {
        if history.len() == HISTORY {
            history.pop_front();
        }
        history.push_back(op);
    };
    push(
        &mut history,
        format!("init {}/{}ns burst {}", r.tokens, r.period_ns, r.burst),
    );

    for step in 0..steps {
        let now = m.now + gen_step(s, &m, steps - step);
        let b = m.burst;
        let kind = s.below(100);
        let (op, want, got): (String, Outcome, Outcome) = if kind < 35 {
            let cost = if s.below(8) == 0 {
                s.below(b + 3)
            } else {
                s.below(4)
            };
            (
                format!("t={now} take {cost}"),
                m.take(now, cost),
                g.take(now, cost),
            )
        } else if kind < 50 {
            let max = s.below(2 * b + 2);
            (
                format!("t={now} take_upto {max}"),
                Ok(m.take_range(now, 1, max)),
                Ok(g.take_upto(now, max)),
            )
        } else if kind < 65 {
            let min = s.below(b + 3);
            let max = s.below(2 * b + 2);
            (
                format!("t={now} take_range {min}..{max}"),
                Ok(m.take_range(now, min, max)),
                Ok(g.take_range(now, min, max)),
            )
        } else if kind < 80 {
            let tokens = if s.below(20) == 0 {
                u64::MAX
            } else {
                s.below(b + 2)
            };
            m.refund(now, tokens);
            g.refund(now, tokens);
            (
                format!("t={now} refund {tokens}"),
                Ok(only_remaining(m.remaining(now))),
                Ok(only_remaining(g.remaining(now))),
            )
        } else if kind < 97 {
            (
                format!("t={now} remaining"),
                Ok(only_remaining(m.remaining(now))),
                Ok(only_remaining(g.remaining(now))),
            )
        } else {
            let nr = gen_rate(s);
            m.advance(now);
            let mut want = m.reconfigure(&nr);
            let got = g.reconfigure(&nr);
            if want.is_ok() && !rate_within_contract(&nr, m.rate) {
                // Forces a report below.
                want = Err(Error::InvalidConfig);
            }
            (
                format!(
                    "t={now} reconfigure {}/{}ns burst {}",
                    nr.tokens, nr.period_ns, nr.burst
                ),
                want.map(|()| only_remaining(m.remaining(now))),
                got.map(|()| only_remaining(g.remaining(now))),
            )
        };
        push(&mut history, op);
        if want != got {
            return Err(Divergence {
                step,
                history: history.into_iter().collect(),
                want,
                got,
            }
            .to_string());
        }
    }
    Ok(())
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}

fn run_seed(seed: u64) -> Result<(), String> {
    run(&mut SplitMix(seed), STEPS).map_err(|e| {
        format!(
            "model_gcra: seed {seed}: {e}\n  replay: EDGEFRONT_MODEL_SEED={seed} \
             cargo test -p edgefront-limiter --test model_gcra"
        )
    })
}

#[test]
fn model_gcra_matches_reference_bucket() {
    if let Some(seed) = env_u64("EDGEFRONT_MODEL_SEED") {
        if let Err(e) = run_seed(seed) {
            panic!("{e}");
        }
        return;
    }
    let seeds = env_u64("EDGEFRONT_MODEL_SEEDS").unwrap_or(DEFAULT_SEEDS);
    let failures: Vec<String> = (1..=seeds)
        .filter_map(|seed| run_seed(seed).err())
        .take(3)
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The same model check on scripts proptest chooses, so a failure
    /// shrinks to a short tape and a short script.
    #[test]
    fn model_gcra_matches_reference_on_shrinkable_scripts(
        words in proptest::collection::vec(any::<u64>(), 0..2_000),
        steps in 1usize..400,
    ) {
        let mut tape = Tape { words: &words, at: 0 };
        if let Err(e) = run(&mut tape, steps) {
            return Err(TestCaseError::fail(format!("model_gcra: {e}")));
        }
    }
}

/// The reference itself, on hand-worked cases, so a broken model cannot
/// pass by agreeing with a broken library.
#[test]
fn model_gcra_reference_by_hand() {
    // 10/s, burst 5: 100 ms a token.
    let r = Rate::new(10, 1_000_000_000, 5);
    let mut m = RefBucket::new(&r, 0).unwrap();
    let d = m.take(0, 5).unwrap();
    assert!(d.allowed);
    assert_eq!(d.remaining, 0);
    let d = m.take(0, 2).unwrap();
    assert!(!d.allowed);
    assert_eq!(d.retry_after_ns, 200_000_000);
    // 1.5 tokens back.
    let d = m.take(150_000_000, 1).unwrap();
    assert!(d.allowed);
    assert_eq!(d.remaining, 0);
    let d = m.take_range(150_000_000, 2, 9);
    assert_eq!(d.granted, 0);
    // From 0.5 to 2 tokens.
    assert_eq!(d.retry_after_ns, 150_000_000);
    m.refund(150_000_000, 100);
    assert_eq!(m.remaining(150_000_000), 5);
    // Halve the rate with 2 tokens of debt: 200 ms of debt stays 200 ms,
    // which is one token at 5/s, so 4 of the new burst of 5 remain.
    assert!(m.take(150_000_000, 2).unwrap().allowed);
    m.reconfigure(&Rate::new(5, 1_000_000_000, 5)).unwrap();
    assert_eq!(m.remaining(150_000_000), 4);
    // Shrinking the burst below the debt leaves a negative level.
    assert!(m.take(150_000_000, 4).unwrap().allowed);
    m.reconfigure(&Rate::new(5, 1_000_000_000, 1)).unwrap();
    assert_eq!(m.level.cmp(Rat::int(0)), Ordering::Less);
    assert_eq!(m.remaining(150_000_000), 0);
    assert_eq!(m.reconfigure(&Rate::new(0, 1, 1)), Err(Error::InvalidRate));
    // 3 per 7 ns rounds 7/3 ns up to 150/64 ns, slower but within 1/150.
    let odd = Rate::new(3, 7, 1);
    let rate = ref_rate(&odd).unwrap();
    assert_eq!(rate, Rat::new(64, 150));
    assert!(rate_within_contract(&odd, rate));
    // A faster rounding would break the contract.
    assert!(!rate_within_contract(&odd, Rat::new(64, 149)));
}

#[test]
fn rationals_reduce_and_order() {
    assert_eq!(Rat::new(6, -4), Rat { num: -3, den: 2 });
    assert_eq!(Rat::new(1, 3).add(Rat::new(1, 6)), Rat::new(1, 2));
    assert_eq!(Rat::new(2, 3).mul(Rat::new(9, 4)), Rat::new(3, 2));
    assert_eq!(Rat::new(1, 2).div(Rat::new(1, 4)), Rat::int(2));
    assert_eq!(Rat::new(1, 3).cmp(Rat::new(1, 2)), Ordering::Less);
    assert_eq!(Rat::new(7, 2).floor_nonneg(), 3);
    assert_eq!(Rat::new(7, 2).ceil_nonneg(), 4);
    assert_eq!(Rat::int(4).ceil_nonneg(), 4);
}

#[test]
fn a_divergence_names_its_step_and_history() {
    let d = Divergence {
        step: 3,
        history: vec!["init 1/1ns burst 1".into(), "t=5 take 1".into()],
        want: Ok(Decision::default()),
        got: Err(Error::CostExceedsBurst),
    };
    let s = d.to_string();
    assert!(s.starts_with("divergence at step 3:"), "{s}");
    assert!(s.contains("  > t=5 take 1"), "{s}");
    assert!(s.contains("library:   Err(CostExceedsBurst)"), "{s}");
}

#[test]
fn an_empty_tape_still_makes_a_valid_script() {
    let mut tape = Tape { words: &[], at: 0 };
    run(&mut tape, 50).unwrap();
}
