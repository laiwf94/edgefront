//! Replaying a recording against a candidate policy: what a tighter (or
//! looser) limit would have done to real traffic, key by key.
//!
//! [`replay`] takes a [`Recording`] and candidate rules. Each recorded
//! rule with a candidate of the same name gets a fresh table at the
//! candidate's rate, burst and table settings, and every record of that
//! rule, in time order, takes its cost from it at its recorded time: a
//! virtual clock, so an hour of traffic replays as fast as the tables
//! decide. The candidate's outcome is set against the recorded one (a
//! shadow rule's would-refuse counts as refused, since that is what it
//! would have done enforcing). The report gives per rule the requests,
//! refusals before and after, and how many flipped each way, and per key
//! (by the recorder's hash: a recording holds no key) the ones whose
//! refusals changed, the largest change first.
//!
//! The candidate decides on the recorded keys, so a candidate can change
//! a rule's rate, burst, cost or mode, not what its key is built from;
//! a recorded rule with no candidate of its name is listed as missing,
//! and a candidate with no records as new.

use std::collections::HashMap;

use serde::Serialize;

use crate::http::layer::HttpRule;
use crate::record::Recording;
use crate::table::{Table, TableConfig};

/// One rule's totals.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RuleDiff {
    pub rule: String,
    pub requests: u64,
    pub refused_before: u64,
    pub refused_after: u64,
    /// Admitted in the recording, refused by the candidate.
    pub newly_refused: u64,
    /// Refused in the recording, admitted by the candidate.
    pub newly_admitted: u64,
    pub keys: u64,
    pub keys_changed: u64,
}

/// One key whose outcome changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct KeyDiff {
    pub rule: String,
    /// The recorder's hash of the key, in hex.
    pub key: String,
    pub requests: u64,
    pub refused_before: u64,
    pub refused_after: u64,
}

/// The whole comparison.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub records: u64,
    pub rules: Vec<RuleDiff>,
    /// Every key whose refusals changed, the largest change first.
    pub keys: Vec<KeyDiff>,
    /// Recorded rules the candidate has no rule for.
    pub missing: Vec<String>,
    /// Candidate rules the recording has no records for.
    pub new: Vec<String>,
}

#[derive(Default)]
struct PerKey {
    requests: u64,
    before: u64,
    after: u64,
}

/// Replays `rec` against `candidate`.
pub fn replay(rec: &Recording, candidate: &[HttpRule]) -> Result<Report, String> {
    let mut report = Report {
        records: rec.records.len() as u64,
        ..Report::default()
    };
    // A table per recorded rule that has a candidate.
    let mut tables: Vec<Option<(Table, &HttpRule)>> = Vec::with_capacity(rec.rules.len());
    for name in &rec.rules {
        match candidate.iter().find(|c| c.name == *name) {
            Some(c) => {
                let mut cfg = TableConfig::new(c.table_size, c.rate);
                cfg.full_policy = c.table_full;
                tables.push(Some((
                    Table::new(&cfg).map_err(|e| format!("http limit {}: {e}", c.name))?,
                    c,
                )));
            }
            None => {
                report.missing.push(name.clone());
                tables.push(None);
            }
        }
    }
    report.new = candidate
        .iter()
        .filter(|c| !rec.rules.contains(&c.name))
        .map(|c| c.name.clone())
        .collect();
    let mut order: Vec<usize> = (0..rec.records.len()).collect();
    order.sort_by_key(|&i| rec.records[i].at_ns);
    let mut rules: Vec<RuleDiff> = rec
        .rules
        .iter()
        .map(|r| RuleDiff {
            rule: r.clone(),
            ..RuleDiff::default()
        })
        .collect();
    let mut keys: Vec<HashMap<[u8; 16], PerKey>> =
        (0..rec.rules.len()).map(|_| HashMap::new()).collect();
    for i in order {
        let r = &rec.records[i];
        let ri = usize::from(r.rule);
        let Some(Some((table, cand))) = tables.get(ri) else {
            continue;
        };
        let after = if cand.mode_enforces() {
            match table.take(&r.key, r.at_ns, u64::from(r.cost)) {
                Ok(d) => !d.allowed,
                Err(crate::gcra::Error::CostExceedsBurst | crate::gcra::Error::TableFull) => true,
                Err(_) => false,
            }
        } else {
            // A candidate that is off never refuses; a shadow one decides
            // as enforcing would.
            match table.take(&r.key, r.at_ns, u64::from(r.cost)) {
                Ok(d) => !d.allowed && cand.mode != crate::shadow::Mode::Off,
                Err(_) => cand.mode != crate::shadow::Mode::Off,
            }
        };
        let before = r.outcome.refused();
        let d = &mut rules[ri];
        d.requests += 1;
        d.refused_before += u64::from(before);
        d.refused_after += u64::from(after);
        d.newly_refused += u64::from(after && !before);
        d.newly_admitted += u64::from(before && !after);
        let k = keys[ri].entry(r.key).or_default();
        k.requests += 1;
        k.before += u64::from(before);
        k.after += u64::from(after);
    }
    for (ri, per) in keys.into_iter().enumerate() {
        rules[ri].keys = per.len() as u64;
        for (k, v) in per {
            if v.before != v.after {
                rules[ri].keys_changed += 1;
                report.keys.push(KeyDiff {
                    rule: rec.rules[ri].clone(),
                    key: k.iter().map(|b| format!("{b:02x}")).collect(),
                    requests: v.requests,
                    refused_before: v.before,
                    refused_after: v.after,
                });
            }
        }
    }
    report.keys.sort_by(|a, b| {
        let d = |k: &KeyDiff| k.refused_after.abs_diff(k.refused_before);
        d(b).cmp(&d(a))
            .then_with(|| a.rule.cmp(&b.rule))
            .then_with(|| a.key.cmp(&b.key))
    });
    report.rules = rules;
    Ok(report)
}

impl Report {
    /// The report as text: a table of rules, then the first `top` keys.
    pub fn text(&self, top: usize) -> String {
        let mut out = format!("{} records\n\n", self.records);
        out.push_str("| rule | requests | refused before | refused after | newly refused | newly admitted | keys | keys changed |\n|---|---:|---:|---:|---:|---:|---:|---:|\n");
        for r in &self.rules {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                r.rule,
                r.requests,
                r.refused_before,
                r.refused_after,
                r.newly_refused,
                r.newly_admitted,
                r.keys,
                r.keys_changed
            ));
        }
        if !self.keys.is_empty() {
            out.push_str(&format!("\n{} keys changed; the first {}:\n\n| rule | key hash | requests | refused before | refused after |\n|---|---|---:|---:|---:|\n", self.keys.len(), top.min(self.keys.len())));
            for k in self.keys.iter().take(top) {
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    k.rule,
                    &k.key[..16],
                    k.requests,
                    k.refused_before,
                    k.refused_after
                ));
            }
        }
        for m in &self.missing {
            out.push_str(&format!(
                "\nrecorded rule {m} has no candidate: not replayed\n"
            ));
        }
        for n in &self.new {
            out.push_str(&format!("\ncandidate rule {n} has no records\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::http_limits::{HttpLimitsSection, build};
    use crate::record::{Outcome, Record};

    fn rules(text: &str) -> Vec<HttpRule> {
        build(
            &toml::from_str::<HttpLimitsSection>(text)
                .unwrap()
                .http_limits,
        )
        .unwrap()
    }

    fn key(n: u8) -> [u8; 16] {
        [n; 16]
    }

    #[test]
    fn a_tighter_policy_lists_the_keys_it_would_refuse() {
        const SEC: u64 = 1_000_000_000;
        // Two keys at 10 a second for 10 s, recorded under a limit of 20
        // a second that refused nothing.
        let mut records = Vec::new();
        for i in 0..100u64 {
            let t = SEC + i * SEC / 10;
            records.push(Record::new(t, 0, key(1), 1, Outcome::Allowed));
            records.push(Record::new(t, 0, key(2), 1, Outcome::Allowed));
        }
        // A third key the recording refused at the end.
        records.push(Record::new(20 * SEC, 0, key(3), 1, Outcome::Refused));
        records.push(Record::new(20 * SEC, 1, key(3), 1, Outcome::Allowed));
        let rec = Recording {
            rules: vec!["api".into(), "gone".into()],
            records,
        };
        let same = rules("[[http_limits]]\nname = \"api\"\nrate = \"20/s burst 20\"\n");
        let r = replay(&rec, &same).unwrap();
        assert_eq!(r.records, 202);
        assert_eq!(r.rules[0].refused_before, 1);
        assert_eq!(r.rules[0].newly_admitted, 1, "key 3 fits the limit");
        assert_eq!(r.missing, ["gone"]);
        let tight = rules(
            "[[http_limits]]\nname = \"api\"\nrate = \"5/s burst 5\"\nmode = \"shadow\"\n[[http_limits]]\nname = \"fresh\"\nrate = \"1/s\"\n",
        );
        let r = replay(&rec, &tight).unwrap();
        let d = &r.rules[0];
        assert_eq!(d.requests, 201);
        // Each key: 5 at once, then one per 200 ms over 9.9 s: 54 of 100.
        assert_eq!(d.refused_after, 2 * 46);
        assert_eq!((d.newly_refused, d.newly_admitted), (92, 1));
        assert_eq!((d.keys, d.keys_changed), (3, 3));
        assert_eq!(r.keys[0].refused_after, 46);
        assert_eq!(r.keys.last().unwrap().refused_before, 1);
        assert_eq!(r.new, ["fresh"]);
        let text = r.text(10);
        assert!(
            text.contains("| api | 201 | 1 | 92 | 92 | 1 | 3 | 3 |"),
            "{text}"
        );
        assert!(text.contains("3 keys changed"));
        assert!(text.contains("recorded rule gone has no candidate"));
        // Off refuses nothing.
        let off = rules("[[http_limits]]\nname = \"api\"\nrate = \"1/s\"\nmode = \"off\"\n");
        assert_eq!(replay(&rec, &off).unwrap().rules[0].refused_after, 0);
    }
}
