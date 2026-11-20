//! Counting on response status: a rule that counts only some responses,
//! such as failed logins.
//!
//! A rule with a `count` list takes its token when the request arrives,
//! like any other, so that a burst of concurrent guesses cannot all slip
//! past while their answers are still on the way. Once the response is
//! known, a request whose status is not on the list is *excluded*: its
//! token goes back with [`crate::Table::refund`]. A password-guessing
//! client that keeps getting `401` keeps its tokens spent and is refused
//! after N; a user whose logins succeed gets every token back.
//!
//! The status counted is the one the client gets: the upstream's, or the
//! one the proxy wrote when there was no usable upstream response (`502`,
//! `503`, `504`). A request whose client went away before the response
//! (the exchange dropped without a delivery) stays counted, so hanging up
//! early is not a way around the limit.
//!
//! A refund never moves a bucket before `now` (see
//! [`crate::Gcra::refund`]), so a client cannot bank tokens by being
//! refunded: the most it can have is a full bucket.

use std::fmt;

/// A set of status codes from 100 to 599, written as codes (`401`) and
/// classes (`"4xx"`).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct StatusSet {
    bits: [u64; 8],
}

impl StatusSet {
    /// No status.
    pub const EMPTY: StatusSet = StatusSet { bits: [0; 8] };

    /// Reads a list of `"401"`, `"4xx"` or `"5XX"` items. A code outside
    /// 100 to 599, a class outside 1xx to 5xx, an item listed twice or an
    /// empty list is refused.
    pub fn parse<'a>(items: impl IntoIterator<Item = &'a str>) -> Result<StatusSet, String> {
        let mut set = StatusSet::EMPTY;
        let mut any = false;
        for item in items {
            any = true;
            let lower = item.trim().to_ascii_lowercase();
            let (lo, hi) = match lower.strip_suffix("xx") {
                Some(class) => match class.parse::<u16>() {
                    Ok(c @ 1..=5) => (c * 100, c * 100 + 99),
                    _ => return Err(format!("{item:?} is not a status class 1xx to 5xx")),
                },
                None => match lower.parse::<u16>() {
                    Ok(code @ 100..=599) => (code, code),
                    _ => return Err(format!("{item:?} is not a status code 100 to 599")),
                },
            };
            if (lo..=hi).all(|c| set.contains(c)) {
                return Err(format!("{item:?} is already counted"));
            }
            for c in lo..=hi {
                set.insert(c);
            }
        }
        if !any {
            return Err("the list is empty; leave count out to count every response".into());
        }
        Ok(set)
    }

    fn insert(&mut self, code: u16) {
        let i = usize::from(code - 100);
        self.bits[i / 64] |= 1 << (i % 64);
    }

    /// Whether `code` is in the set.
    #[inline]
    pub fn contains(&self, code: u16) -> bool {
        if !(100..600).contains(&code) {
            return false;
        }
        let i = usize::from(code - 100);
        self.bits[i / 64] & (1 << (i % 64)) != 0
    }

    /// How many codes the set holds.
    pub fn len(&self) -> u32 {
        self.bits.iter().map(|w| w.count_ones()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The set in the shortest list of items: whole classes as `4xx`,
    /// the rest as codes, in order.
    pub fn items(&self) -> Vec<String> {
        let mut out = Vec::new();
        for class in 1..=5u16 {
            let lo = class * 100;
            if (lo..lo + 100).all(|c| self.contains(c)) {
                out.push(format!("{class}xx"));
                continue;
            }
            out.extend(
                (lo..lo + 100)
                    .filter(|&c| self.contains(c))
                    .map(|c| c.to_string()),
            );
        }
        out
    }
}

impl fmt::Debug for StatusSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.items()).finish()
    }
}

/// What happens to a request's token once its response is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settle {
    /// The status is counted, or the rule counts every response.
    Keep,
    /// The status is excluded: refund the token.
    Refund,
}

/// The settlement for a response with `status` under a rule counting
/// `count` (`None`: every response).
#[inline]
pub fn settle(count: Option<&StatusSet>, status: u16) -> Settle {
    match count {
        Some(set) if !set.contains(status) => Settle::Refund,
        _ => Settle::Keep,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_classes_parse() {
        let s = StatusSet::parse(["401", "403"]).unwrap();
        assert!(s.contains(401) && s.contains(403));
        assert!(!s.contains(200) && !s.contains(402) && !s.contains(404));
        assert_eq!(s.len(), 2);
        assert_eq!(s.items(), ["401", "403"]);
        let s = StatusSet::parse(["5XX", "429"]).unwrap();
        assert!(s.contains(500) && s.contains(599) && s.contains(429));
        assert!(!s.contains(499) && !s.contains(600));
        assert_eq!(s.items(), ["429", "5xx"]);
        assert_eq!(format!("{s:?}"), "[\"429\", \"5xx\"]");
        assert!(!s.contains(0) && !s.contains(99) && !s.contains(u16::MAX));
        let all = StatusSet::parse(["1xx", "2xx", "3xx", "4xx", "5xx"]).unwrap();
        assert_eq!(all.len(), 500);
        assert!(!StatusSet::EMPTY.contains(200));
        assert!(StatusSet::EMPTY.is_empty() && !all.is_empty());
    }

    #[test]
    fn bad_lists_are_refused() {
        for (items, want) in [
            (vec!["99"], "\"99\" is not a status code 100 to 599"),
            (vec!["600"], "\"600\" is not a status code 100 to 599"),
            (vec!["abc"], "\"abc\" is not a status code 100 to 599"),
            (vec!["6xx"], "\"6xx\" is not a status class 1xx to 5xx"),
            (vec!["xx"], "\"xx\" is not a status class 1xx to 5xx"),
            (vec!["401", "401"], "\"401\" is already counted"),
            (vec!["4xx", "404"], "\"404\" is already counted"),
            (
                vec![],
                "the list is empty; leave count out to count every response",
            ),
        ] {
            assert_eq!(StatusSet::parse(items).unwrap_err(), want);
        }
        // A code then its class is fine: the class adds the rest.
        assert_eq!(StatusSet::parse(["404", "4xx"]).unwrap().len(), 100);
    }

    #[test]
    fn excluded_statuses_are_refunded() {
        let s = StatusSet::parse(["401", "403"]).unwrap();
        assert_eq!(settle(Some(&s), 401), Settle::Keep);
        assert_eq!(settle(Some(&s), 200), Settle::Refund);
        assert_eq!(settle(Some(&s), 502), Settle::Refund);
        assert_eq!(settle(None, 200), Settle::Keep);
        assert_eq!(settle(None, 502), Settle::Keep);
    }
}
