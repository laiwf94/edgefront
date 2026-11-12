//! Rates as operators write them: fractions and units from seconds to days.
//!
//! pacerd took whole counts per second, minute or hour only (`10/s`,
//! `600/m`). A [`RateSpec`] also takes decimal counts and days, with an
//! optional number of units in the period:
//!
//! | text | meaning | enforced by |
//! |---|---|---|
//! | `0.5/s` | one every two seconds, burst 1 | token bucket |
//! | `90/min` | 90 a minute, burst 90 | token bucket |
//! | `2.5/10s burst 5` | 2.5 every ten seconds, burst 5 | token bucket |
//! | `100/h` | at most 100 in any hour | window quota |
//! | `10000/day` | at most 10,000 in any day | window quota |
//! | `1.5/h` | at most 3 in any two hours | window quota |
//!
//! Counts take up to nine decimal places and pacer's `k`, `m` and `g`
//! multipliers (powers of 1024, so `1m/s` is still a MiB a second). Units
//! are `s`, `sec`, `second`; `m`, `min`, `minute`; `h`, `hr`, `hour`; `d`,
//! `day`, each also in the plural. A count is held as an exact fraction of
//! its period, never as a float, so `0.1/s` is exactly one per ten seconds.
//!
//! **Long windows become quotas.** A period of an hour or more without an
//! explicit burst is a quota, "at most N in any window", enforced by the
//! sliding [`Window`] counter in sixteen buckets. A token bucket would
//! admit a full burst and then the refill on top of it, up to twice the
//! count in one window; the window counter holds any window to the count,
//! within one bucket's weighting. A fractional count is scaled up to the
//! shortest whole window: `1.5/h` is 3 in 2 hours, `0.5/day` is 1 in 2
//! days. Shorter periods, and any rate with `burst`, are token buckets on
//! [`Gcra`].
//!
//! **Every accepted rate is enforced within 0.1%.** The bucket's time per
//! token is rounded up to a whole tick of 1/64 ns, so it never grants
//! faster than written and is slower by less than 1/interval. A rate fast
//! enough for that to exceed 0.1% (above about 64 million a second) is
//! refused rather than silently slowed, and so is one whose exact interval
//! or quota window does not fit 64 bits or is longer than [`MAX_PERIOD_NS`].

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

use crate::gcra::{Decision, Error, Gcra, Rate, TICK_SHIFT};
use crate::window::Window;

const SEC: u64 = 1_000_000_000;
/// The longest period or quota window a rate may have: 366 days.
pub const MAX_PERIOD_NS: u64 = 366 * 86_400 * SEC;
/// Periods at least this long, without an explicit burst, are quotas.
pub const QUOTA_MIN_PERIOD_NS: u64 = 3_600 * SEC;
/// The largest count or burst, as in pacerd's sizes: 2^50.
pub const MAX_COUNT: u64 = 1 << 50;
/// Decimal places a count may have.
pub const MAX_SCALE: u32 = 9;
/// Buckets a quota's window is cut into.
pub const QUOTA_SLOTS: u32 = crate::window::SLOTS_MAX;
/// The worst slowdown a bucket's rounding may cause, as a fraction:
/// 1/`MAX_ERROR_DEN`, that is 0.1%.
pub const MAX_ERROR_DEN: u128 = 1_000;

/// The unit a period is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Unit {
    Second,
    Minute,
    Hour,
    Day,
}

impl Unit {
    const NAMES: [(&'static str, Unit); 15] = [
        ("s", Unit::Second),
        ("sec", Unit::Second),
        ("secs", Unit::Second),
        ("second", Unit::Second),
        ("seconds", Unit::Second),
        ("m", Unit::Minute),
        ("min", Unit::Minute),
        ("mins", Unit::Minute),
        ("minute", Unit::Minute),
        ("minutes", Unit::Minute),
        ("h", Unit::Hour),
        ("hr", Unit::Hour),
        ("hour", Unit::Hour),
        ("d", Unit::Day),
        ("day", Unit::Day),
    ];

    fn from_name(name: &str) -> Option<Unit> {
        match name {
            "hrs" | "hours" => return Some(Unit::Hour),
            "days" => return Some(Unit::Day),
            _ => {}
        }
        Self::NAMES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|&(_, u)| u)
    }

    /// The unit's length in nanoseconds.
    pub const fn ns(self) -> u64 {
        match self {
            Unit::Second => SEC,
            Unit::Minute => 60 * SEC,
            Unit::Hour => 3_600 * SEC,
            Unit::Day => 86_400 * SEC,
        }
    }

    /// The name [`RateSpec`]'s `Display` writes.
    pub const fn as_str(self) -> &'static str {
        match self {
            Unit::Second => "s",
            Unit::Minute => "min",
            Unit::Hour => "h",
            Unit::Day => "day",
        }
    }
}

/// Why a rate's text was refused. The message names the text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateError {
    pub message: String,
}

impl fmt::Display for RateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RateError {}

fn err(message: String) -> RateError {
    RateError { message }
}

/// How a [`RateSpec`] is enforced. See the [module docs](self).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Plan {
    /// A token bucket on [`Gcra`] (or a [`crate::Table`] entry).
    Bucket(Rate),
    /// At most `limit` in any `window_ns`, on a [`Window`] of
    /// [`QUOTA_SLOTS`] buckets.
    Quota { limit: u64, window_ns: u64 },
}

impl Plan {
    /// A limiter for this plan, with a full bucket or an empty window
    /// starting at `now_ns`.
    pub fn limiter(&self, now_ns: u64) -> Limiter {
        match *self {
            // A plan is only built from a rate whose ticks were checked.
            Plan::Bucket(rate) => Limiter::Bucket(Gcra::new(&rate).expect("checked rate")),
            Plan::Quota { limit, window_ns } => {
                Limiter::Quota(Quota::new(limit, window_ns, now_ns).expect("checked quota"))
            }
        }
    }
}

/// A rate as written: an exact decimal count per period, and a burst.
/// Parse one with [`RateSpec::parse`] or `str::parse`; `Display` writes a
/// canonical form that parses back to an equal value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RateSpec {
    /// The count is `mantissa / 10^scale`, with no trailing zero digits.
    mantissa: u128,
    scale: u32,
    /// Units in the period: `100/10s` has `every` 10.
    every: u64,
    unit: Unit,
    burst: Option<u64>,
}

/// `num` without trailing decimal zeros, as (mantissa, scale).
fn normalize(mut mantissa: u128, mut scale: u32) -> (u128, u32) {
    while scale > 0 && mantissa.is_multiple_of(10) {
        mantissa /= 10;
        scale -= 1;
    }
    (mantissa, scale)
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// The multiplier for pacer's size suffixes.
fn multiplier(suffix: &str) -> Option<u128> {
    match suffix {
        "" => Some(1),
        "k" => Some(1 << 10),
        "m" => Some(1 << 20),
        "g" => Some(1 << 30),
        _ => None,
    }
}

/// `12`, `0.5`, `1.5k`: an exact decimal with an optional multiplier, as
/// (mantissa, scale). None when it is not one. A count with more than
/// [`MAX_SCALE`] decimal places or more than 20 whole digits comes back as
/// `u128::MAX` with its scale, for the caller to report.
fn parse_count(s: &str) -> Option<(u128, u32)> {
    let digits_end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num, suffix) = s.split_at(digits_end);
    let mul = multiplier(suffix)?;
    let (whole, frac) = match num.split_once('.') {
        Some((_, "")) => return None,
        Some(parts) => parts,
        None => (num, ""),
    };
    let all_digits = |p: &str| p.bytes().all(|c| c.is_ascii_digit());
    if whole.is_empty() || !all_digits(whole) || !all_digits(frac) {
        return None;
    }
    let scale = u32::try_from(frac.len()).ok()?;
    let whole = whole.trim_start_matches('0');
    if whole.len() > 20 || scale > MAX_SCALE {
        return Some((u128::MAX, scale));
    }
    // Below 10^20 * 10^9 * 2^30, well inside u128.
    let digits = |p: &str| p.parse::<u128>().unwrap_or(0);
    let mantissa = digits(whole) * 10u128.pow(scale) + digits(frac);
    Some((mantissa * mul, scale))
}

/// `s` in double quotes, cut to 44 bytes, for messages.
fn quote(s: &str) -> String {
    const SHOWN_MAX: usize = 44;
    if s.len() <= SHOWN_MAX {
        return format!("{s:?}");
    }
    let cut = (0..=SHOWN_MAX).rev().find(|&i| s.is_char_boundary(i));
    format!("{:?}...", &s[..cut.unwrap_or(0)])
}

/// `s`, `10s`, `min`, `2h`, `day`: (units, unit).
fn parse_period(s: &str) -> Option<(u64, Unit)> {
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    let (n, name) = s.split_at(digits);
    let every = if n.is_empty() {
        1
    } else if n.len() > 9 {
        return None;
    } else {
        n.parse().ok().filter(|&e| e != 0)?
    };
    Some((every, Unit::from_name(name)?))
}

/// The share by which a bucket with this rate is slower than written, as
/// the exact fraction `(slow, of)`: the interval in ticks is rounded up,
/// so the enforced time per `tokens` tokens is `iv * tokens` ticks against
/// a written `period * 64`. None when the rate is invalid.
pub fn bucket_slowdown(rate: &Rate) -> Option<(u128, u128)> {
    let (iv, _) = rate.ticks().ok()?;
    let written = u128::from(rate.period_ns) << TICK_SHIFT;
    let enforced = u128::from(iv) * u128::from(rate.tokens);
    Some((enforced - written, written))
}

impl RateSpec {
    /// Parse `COUNT/PERIOD` or `COUNT/PERIOD burst N`, such as `0.5/s`,
    /// `90/min`, `10000/day` or `2.5/10s burst 5`. Words may be separated
    /// by any whitespace. Every check of the [module docs](self) is made
    /// here, so [`RateSpec::plan`] cannot fail.
    pub fn parse(text: &str) -> Result<Self, RateError> {
        let words: Vec<&str> = text.split_whitespace().collect();
        let burst = match words.as_slice() {
            [_] => None,
            [_, b, n] if *b == "burst" => Some(
                parse_count(n)
                    .filter(|&(m, scale)| scale == 0 && (1..=u128::from(MAX_COUNT)).contains(&m))
                    .map(|(m, _)| m as u64)
                    .ok_or_else(|| {
                        err(format!(
                            "expected a burst of 1 to 2^50 like 20 or 64k, got {}",
                            quote(n)
                        ))
                    })?,
            ),
            [] => {
                return Err(err(
                    "expected a rate like 0.5/s, 90/min or 10000/day, got nothing".into(),
                ));
            }
            _ => {
                return Err(err(format!(
                    "expected RATE or RATE burst N, like 10/s burst 20, got {}",
                    quote(text.trim())
                )));
            }
        };
        let rate = words[0];
        let bad = || {
            err(format!(
                "expected a rate like 0.5/s, 90/min or 10000/day, got {}",
                quote(rate)
            ))
        };
        let (count, period) = rate.split_once('/').ok_or_else(bad)?;
        let (mantissa, scale) = parse_count(count).ok_or_else(bad)?;
        let (every, unit) = parse_period(period).ok_or_else(bad)?;
        let fail = |why: String| err(format!("{}: {why}", quote(rate)));
        if scale > MAX_SCALE {
            return Err(fail(format!(
                "a count may have at most {MAX_SCALE} decimal places"
            )));
        }
        let (mantissa, scale) = normalize(mantissa, scale);
        if mantissa == 0 {
            return Err(fail("a rate must be above zero".into()));
        }
        if mantissa > u128::from(MAX_COUNT) * 10u128.pow(scale) {
            return Err(fail("the count is above 2^50".into()));
        }
        if u128::from(every) * u128::from(unit.ns()) > u128::from(MAX_PERIOD_NS) {
            return Err(fail("the period is longer than 366 days".into()));
        }
        let spec = Self {
            mantissa,
            scale,
            every,
            unit,
            burst,
        };
        spec.check().map_err(fail)?;
        Ok(spec)
    }

    /// The checks that make [`RateSpec::plan`] infallible.
    fn check(&self) -> Result<(), String> {
        match self.plan_unchecked()? {
            Plan::Bucket(rate) => {
                rate.ticks()
                    .map_err(|_| "rate and burst are too large together".to_string())?;
                let (slow, of) = bucket_slowdown(&rate).expect("valid rate");
                if slow * MAX_ERROR_DEN > of {
                    return Err(format!(
                        "too fast to enforce within 0.1% (it would run {:.2}% slow)",
                        slow as f64 * 100.0 / of as f64
                    ));
                }
                Ok(())
            }
            Plan::Quota { .. } => Ok(()),
        }
    }

    fn plan_unchecked(&self) -> Result<Plan, String> {
        let period = u128::from(self.every) * u128::from(self.unit.ns());
        let pow = 10u128.pow(self.scale);
        if self.burst.is_none() && period >= u128::from(QUOTA_MIN_PERIOD_NS) {
            // The shortest whole multiple of the period that holds a whole
            // count.
            let g = gcd(self.mantissa, pow);
            let window = period * (pow / g);
            if window > u128::from(MAX_PERIOD_NS) {
                return Err(format!(
                    "below one per 366 days: it needs a window of {} days",
                    window / u128::from(Unit::Day.ns())
                ));
            }
            return Ok(Plan::Quota {
                limit: (self.mantissa / g) as u64,
                window_ns: window as u64,
            });
        }
        let period = period * pow;
        let g = gcd(self.mantissa, period);
        let (tokens, period) = (self.mantissa / g, period / g);
        if period > tokens * u128::from(MAX_PERIOD_NS) {
            return Err("below one per 366 days".to_string());
        }
        let tokens =
            u64::try_from(tokens).map_err(|_| "too precise to hold exactly".to_string())?;
        let period = u64::try_from(period).map_err(|_| {
            "too precise to hold exactly for its period; use fewer decimal places".to_string()
        })?;
        Ok(Plan::Bucket(Rate::new(tokens, period, self.burst())))
    }

    /// How this rate is enforced. See the [module docs](self).
    pub fn plan(&self) -> Plan {
        self.plan_unchecked().expect("checked when parsed")
    }

    /// The burst: as written, or else the count rounded up (at least 1).
    pub fn burst(&self) -> u64 {
        self.burst.unwrap_or_else(|| {
            let ceil = self.mantissa.div_ceil(10u128.pow(self.scale));
            // The count is at most 2^50.
            ceil.max(1) as u64
        })
    }

    /// Whether `burst` was written.
    pub fn has_burst(&self) -> bool {
        self.burst.is_some()
    }

    /// The count per period as the exact fraction `(mantissa, 10^scale)`.
    pub fn count(&self) -> (u128, u128) {
        (self.mantissa, 10u128.pow(self.scale))
    }

    /// The period as written, in nanoseconds.
    pub fn period_ns(&self) -> u64 {
        // Checked against MAX_PERIOD_NS when parsed.
        self.every * self.unit.ns()
    }

    /// The unit the period is written in.
    pub fn unit(&self) -> Unit {
        self.unit
    }

    /// The rate in events per second, for messages and lints.
    pub fn per_second(&self) -> f64 {
        let (num, den) = self.count();
        num as f64 / den as f64 * SEC as f64 / self.period_ns() as f64
    }
}

impl fmt::Display for RateSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pow = 10u128.pow(self.scale);
        write!(f, "{}", self.mantissa / pow)?;
        if self.scale > 0 {
            let frac = self.mantissa % pow;
            write!(f, ".{frac:0width$}", width = self.scale as usize)?;
        }
        f.write_str("/")?;
        if self.every != 1 {
            write!(f, "{}", self.every)?;
        }
        f.write_str(self.unit.as_str())?;
        if let Some(b) = self.burst {
            write!(f, " burst {b}")?;
        }
        Ok(())
    }
}

impl FromStr for RateSpec {
    type Err = RateError;

    fn from_str(s: &str) -> Result<Self, RateError> {
        Self::parse(s)
    }
}

impl Serialize for RateSpec {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RateSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = RateSpec;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a rate like 0.5/s, 90/min or 10000/day")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<RateSpec, E> {
                RateSpec::parse(v).map_err(E::custom)
            }
        }
        d.deserialize_str(V)
    }
}

/// At most `limit` events in any window of `window_ns`, counted by a
/// [`Window`] of [`QUOTA_SLOTS`] buckets. Like the window it belongs to one
/// owner: its methods take `&mut self`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quota {
    limit: u64,
    window: Window,
}

impl Quota {
    /// An empty quota whose window starts at `now_ns`. `limit` must be at
    /// least 1 and `window_ns` at least [`QUOTA_SLOTS`].
    pub fn new(limit: u64, window_ns: u64, now_ns: u64) -> Result<Self, Error> {
        if limit == 0 {
            return Err(Error::InvalidRate);
        }
        Ok(Self {
            limit,
            window: Window::new(window_ns, QUOTA_SLOTS, now_ns)?,
        })
    }

    /// Take `cost` events at `now_ns`, all or nothing. A denied take
    /// records nothing, and its `retry_after_ns` is the exact wait until
    /// the same cost fits. A cost of 0 is always allowed.
    pub fn take(&mut self, now_ns: u64, cost: u64) -> Result<Decision, Error> {
        if cost > self.limit {
            return Err(Error::CostExceedsBurst);
        }
        let used = self.window.count(now_ns);
        let room = self.limit.saturating_sub(used);
        if cost <= room {
            self.window.add(now_ns, cost);
            return Ok(Decision {
                allowed: true,
                granted: cost,
                remaining: room - cost,
                retry_after_ns: 0,
            });
        }
        Ok(Decision {
            allowed: false,
            granted: 0,
            remaining: room,
            retry_after_ns: self.window.until_at_most(now_ns, self.limit - cost),
        })
    }

    /// Events that would still be allowed at `now_ns`.
    pub fn remaining(&mut self, now_ns: u64) -> u64 {
        self.limit.saturating_sub(self.window.count(now_ns))
    }

    /// The most events in one window.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// The window's length in nanoseconds.
    pub fn window_ns(&self) -> u64 {
        self.window.window_ns()
    }
}

/// A limiter for one [`Plan`]: a token bucket or a window quota.
#[derive(Debug)]
pub enum Limiter {
    Bucket(Gcra),
    Quota(Quota),
}

impl Limiter {
    /// Take `cost` at `now_ns`, all or nothing; see [`Gcra::take`] and
    /// [`Quota::take`].
    pub fn take(&mut self, now_ns: u64, cost: u64) -> Result<Decision, Error> {
        match self {
            Limiter::Bucket(g) => g.take(now_ns, cost),
            Limiter::Quota(q) => q.take(now_ns, cost),
        }
    }

    /// What is still available at `now_ns`.
    pub fn remaining(&mut self, now_ns: u64) -> u64 {
        match self {
            Limiter::Bucket(g) => g.remaining(now_ns),
            Limiter::Quota(q) => q.remaining(now_ns),
        }
    }
}

#[cfg(test)]
mod tests;
