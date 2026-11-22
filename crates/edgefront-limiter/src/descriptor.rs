//! Descriptor trees as envoyproxy/ratelimit reads them: domains of nested
//! descriptors with a key, an optional value and a limit, loaded from the
//! service's own YAML, and the service's decisions on them. This is the
//! tree the rate limit service front end (limiter week 10) answers from.
//!
//! ```yaml
//! domain: api
//! descriptors:
//!   - key: remote_address          # any value, a counter per value
//!     rate_limit: {unit: second, requests_per_unit: 10}
//!   - key: path
//!     value: /login                # this value only
//!     rate_limit: {unit: minute, requests_per_unit: 5, name: login}
//!     descriptors:
//!       - key: user
//!         value: "bot-*"           # a wildcard value
//!         rate_limit: {unit: minute, requests_per_unit: 1, replaces: [{name: login}]}
//!   - key: path
//!     value: /health               # listed with no limit: never limited
//!   - key: tenant
//!     rate_limit: {unlimited: true}
//!     shadow_mode: true            # counted, never enforced
//! ```
//!
//! **Matching** ([`DescriptorConfig::get_limit`]) walks a request's
//! descriptor entries down the tree as the service's `GetLimit` does: at
//! each level the child for `key_value` if there is one, else the first
//! wildcard value that matches (`*` stands for any run of characters,
//! several allowed), else the child for `key` alone; no child means no
//! limit. The limit is the one on the node the last entry reaches, if it
//! has one, so a request whose entries stop early, or go deeper than the
//! tree, is not limited.
//!
//! **Decisions** ([`DescriptorLimiter::should_rate_limit`]) are the
//! service's fixed windows: each descriptor's counter is keyed by the
//! domain and every entry's key and value (a wildcard's pattern instead
//! with `share_threshold`), and by the window, the unit's length counted
//! from the Unix epoch. The request's hits are added whether or not it is
//! over, as the service's `INCRBY` does, and a descriptor is over its
//! limit when the counter passes `requests_per_unit`. The response is
//! over the limit when any descriptor is, except those in shadow mode.
//! Before counting, a limit named in another matched limit's `replaces`
//! is dropped. Times are whole Unix seconds, since windows are aligned to
//! the calendar.
//!
//! **Loading** ([`DescriptorConfig::load`]) gives the service's own
//! messages for its mistakes, prefixed by the file name, such as
//! `bad.yaml: invalid rate limit unit 'foo'` or `dup.yaml: duplicate
//! descriptor composite key 'test-domain.key1_value1'`, so a config
//! written for envoyproxy/ratelimit loads unchanged or fails the same way.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};

mod yaml;
pub use yaml::{Node, YamlError, parse as parse_yaml};

/// The unit of a limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Unit {
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
}

impl Unit {
    fn parse(s: &str) -> Option<Unit> {
        Some(match s.to_ascii_lowercase().as_str() {
            "second" => Unit::Second,
            "minute" => Unit::Minute,
            "hour" => Unit::Hour,
            "day" => Unit::Day,
            "week" => Unit::Week,
            "month" => Unit::Month,
            "year" => Unit::Year,
            _ => return None,
        })
    }

    /// The window length in seconds, as the service divides time: a
    /// month is 30 days and a year 365.
    pub fn secs(self) -> u64 {
        match self {
            Unit::Second => 1,
            Unit::Minute => 60,
            Unit::Hour => 3600,
            Unit::Day => 86_400,
            Unit::Week => 7 * 86_400,
            Unit::Month => 30 * 86_400,
            Unit::Year => 365 * 86_400,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Unit::Second => "SECOND",
            Unit::Minute => "MINUTE",
            Unit::Hour => "HOUR",
            Unit::Day => "DAY",
            Unit::Week => "WEEK",
            Unit::Month => "MONTH",
            Unit::Year => "YEAR",
        }
    }
}

/// A descriptor's limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimit {
    /// The tree path, `domain.key1_value1.subkey1`, as the service's
    /// `FullKey` names it.
    pub full_key: String,
    pub name: Option<String>,
    /// `None` only when unlimited.
    pub unit: Option<Unit>,
    /// Units per window: a window of `unit_multiplier` units, 1 for
    /// every configured limit and more only for a request's override.
    pub unit_multiplier: u32,
    pub requests_per_unit: u64,
    pub unlimited: bool,
    pub shadow_mode: bool,
    pub replaces: Vec<String>,
    pub detailed_metric: bool,
    pub share_threshold: bool,
}

#[derive(Debug, Default)]
struct Tree {
    limit: Option<RateLimit>,
    /// Children by `key_value` or `key`.
    children: HashMap<String, Tree>,
    /// Children with a wildcard value: key, pattern, child, in config
    /// order.
    wildcards: Vec<(String, String, Tree)>,
}

/// Every domain's tree.
#[derive(Debug, Default)]
pub struct DescriptorConfig {
    domains: HashMap<String, Tree>,
}

/// The keys the service accepts anywhere in a file.
const VALID_KEYS: &[&str] = &[
    "domain",
    "key",
    "value",
    "descriptors",
    "rate_limit",
    "unit",
    "requests_per_unit",
    "unlimited",
    "name",
    "replaces",
    "shadow_mode",
    "detailed_metric",
    "value_to_metric",
    "share_threshold",
];

fn check_keys(n: &Node, file: &str) -> Result<(), String> {
    match n {
        Node::Map { entries, .. } => {
            for (k, v) in entries {
                if k.parse::<f64>().is_ok() || k == "true" || k == "false" {
                    return Err(format!(
                        "{file}: config error, key is not of type string: {k}"
                    ));
                }
                if !VALID_KEYS.contains(&k.as_str()) {
                    return Err(format!("{file}: config error, unknown key '{k}'"));
                }
                check_keys(v, file)?;
            }
        }
        Node::Seq { items, .. } => {
            for i in items {
                if !matches!(i, Node::Map { .. }) {
                    return Err(format!(
                        "{file}: config error, yaml file contains list of type other than map: {}",
                        i.describe()
                    ));
                }
                check_keys(i, file)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn get<'n>(entries: &'n [(String, Node)], key: &str) -> Option<&'n Node> {
    entries
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .filter(|v| !matches!(v, Node::Null { .. }))
}

fn text<'n>(entries: &'n [(String, Node)], key: &str) -> &'n str {
    get(entries, key).and_then(Node::as_str).unwrap_or("")
}

fn flag(entries: &[(String, Node)], key: &str) -> bool {
    matches!(text(entries, key), "true" | "True" | "TRUE")
}

/// Whether `value` matches `pattern`, `*` standing for any run.
pub fn wildcard_match(pattern: &str, value: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if parts.len() == 1 {
        return pattern == value;
    }
    if !value.starts_with(first) || value.len() < first.len() + last.len() || !value.ends_with(last)
    {
        return false;
    }
    let mut rest = &value[first.len()..value.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(at) => rest = &rest[at + mid.len()..],
            None => return false,
        }
    }
    true
}

impl DescriptorConfig {
    /// Loads `files`, each `(name, text)`. Two files with one domain are
    /// refused unless `merge`, which joins their descriptors (the
    /// service's `MERGE_DOMAIN_CONFIG`).
    pub fn load(files: &[(&str, &str)], merge: bool) -> Result<Self, String> {
        let mut cfg = DescriptorConfig::default();
        for (file, body) in files {
            let doc =
                yaml::parse(body).map_err(|e| format!("{file}: error loading config file: {e}"))?;
            check_keys(&doc, file)?;
            let entries = match &doc {
                Node::Map { entries, .. } => entries.as_slice(),
                Node::Null { .. } => &[],
                _ => return Err(format!("{file}: error loading config file: not a map")),
            };
            let domain = text(entries, "domain");
            if domain.is_empty() {
                return Err(format!("{file}: config file cannot have empty domain"));
            }
            if cfg.domains.contains_key(domain) && !merge {
                return Err(format!(
                    "{file}: duplicate domain '{domain}' in config file"
                ));
            }
            let tree = cfg.domains.entry(domain.to_owned()).or_default();
            if let Some(list) = get(entries, "descriptors") {
                load_level(tree, list, domain, file)?;
            }
        }
        Ok(cfg)
    }

    /// The domains loaded.
    pub fn domains(&self) -> impl Iterator<Item = &str> {
        self.domains.keys().map(String::as_str)
    }

    /// The limit for one descriptor's `entries` in `domain`, as the
    /// service's `GetLimit` finds it; `None` when no limit applies. A
    /// limit with `detailed_metric` is named by the request's own values
    /// (`domain.key7_some_value`), as the service names its stats.
    pub fn get_limit(&self, domain: &str, entries: &[(&str, &str)]) -> Option<RateLimit> {
        let (limit, _) = self.find(domain, entries)?;
        let mut limit = limit.clone();
        if limit.detailed_metric {
            let mut key = domain.to_owned();
            for (k, v) in entries {
                key.push('.');
                key.push_str(k);
                key.push('_');
                key.push_str(v);
            }
            limit.full_key = key;
        }
        Some(limit)
    }

    /// The limit, and whether the walk passed a wildcard with
    /// `share_threshold` (whose pattern then keys the counter).
    fn find(
        &self,
        domain: &str,
        entries: &[(&str, &str)],
    ) -> Option<(&RateLimit, Vec<Option<&str>>)> {
        let mut node = self.domains.get(domain)?;
        let mut shared = Vec::with_capacity(entries.len());
        let mut limit = None;
        for (i, (k, v)) in entries.iter().enumerate() {
            let exact = node.children.get(&format!("{k}_{v}"));
            let (next, share) = match exact {
                Some(n) => (n, None),
                None => match node
                    .wildcards
                    .iter()
                    .find(|(wk, p, _)| wk == k && wildcard_match(p, v))
                {
                    Some((_, p, n)) => (n, Some(p.as_str())),
                    None => (node.children.get(*k)?, None),
                },
            };
            shared.push(share.filter(|_| next.limit.as_ref().is_some_and(|l| l.share_threshold)));
            if i + 1 == entries.len() {
                limit = next.limit.as_ref();
            }
            node = next;
        }
        limit.map(|l| (l, shared))
    }
}

fn load_level(tree: &mut Tree, list: &Node, parent: &str, file: &str) -> Result<(), String> {
    let Node::Seq { items, .. } = list else {
        return Err(format!("{file}: config error, descriptors must be a list"));
    };
    for item in items {
        let Node::Map { entries, .. } = item else {
            unreachable!("check_keys refused non-map lists");
        };
        let key = text(entries, "key");
        if key.is_empty() {
            return Err(format!("{file}: descriptor has empty key"));
        }
        let value = text(entries, "value");
        let composite = if value.is_empty() {
            key.to_owned()
        } else {
            format!("{key}_{value}")
        };
        let full_key = format!("{parent}.{composite}");
        let wild = value.contains('*');
        let taken = if wild {
            tree.wildcards
                .iter()
                .any(|(k, p, _)| k == key && p == value)
        } else {
            tree.children.contains_key(&composite)
        };
        if taken {
            return Err(format!(
                "{file}: duplicate descriptor composite key '{full_key}'"
            ));
        }
        let mut child = Tree::default();
        if let Some(Node::Map { entries: rl, .. }) = get(entries, "rate_limit") {
            let unlimited = flag(rl, "unlimited");
            let unit_text = text(rl, "unit");
            let unit = if unlimited {
                if !unit_text.is_empty() {
                    return Err(format!(
                        "{file}: should not specify rate limit unit when unlimited"
                    ));
                }
                None
            } else {
                Some(
                    Unit::parse(unit_text)
                        .ok_or_else(|| format!("{file}: invalid rate limit unit '{unit_text}'"))?,
                )
            };
            let rpu = text(rl, "requests_per_unit");
            let requests_per_unit = if rpu.is_empty() {
                0
            } else {
                rpu.parse::<u64>().map_err(|_| {
                    format!("{file}: config error, requests_per_unit is not a whole number: {rpu}")
                })?
            };
            let name = Some(text(rl, "name").to_owned()).filter(|n| !n.is_empty());
            let mut replaces = Vec::new();
            if let Some(Node::Seq { items, .. }) = get(rl, "replaces") {
                for r in items {
                    let Node::Map { entries: re, .. } = r else {
                        continue;
                    };
                    let n = text(re, "name");
                    if n.is_empty() {
                        return Err(format!("{file}: should not have an empty replaces entry"));
                    }
                    if name.as_deref() == Some(n) {
                        return Err(format!(
                            "{file}: replaces should not contain name of same descriptor"
                        ));
                    }
                    replaces.push(n.to_owned());
                }
            }
            child.limit = Some(RateLimit {
                full_key: full_key.clone(),
                name,
                unit,
                unit_multiplier: 1,
                requests_per_unit,
                unlimited,
                shadow_mode: flag(entries, "shadow_mode"),
                replaces,
                detailed_metric: flag(entries, "detailed_metric"),
                share_threshold: flag(entries, "share_threshold"),
            });
        }
        if let Some(sub) = get(entries, "descriptors") {
            load_level(&mut child, sub, &full_key, file)?;
        }
        if wild {
            tree.wildcards
                .push((key.to_owned(), value.to_owned(), child));
        } else {
            tree.children.insert(composite, child);
        }
    }
    Ok(())
}

/// One descriptor's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescriptorStatus {
    pub over_limit: bool,
    /// The limit that applied, `None` when none did.
    pub limit: Option<RateLimit>,
    pub limit_remaining: u64,
    /// Seconds until the window ends.
    pub reset_secs: u64,
}

/// A decision on every descriptor of a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub over_limit: bool,
    pub statuses: Vec<DescriptorStatus>,
}

/// The fixed-window counters, sharded, bounded in entries.
struct Counters {
    shards: Vec<Mutex<HashMap<String, (u64, u64)>>>,
    per_shard: usize,
}

impl Counters {
    fn new(max: usize) -> Counters {
        let n = 64;
        Counters {
            shards: (0..n).map(|_| Mutex::new(HashMap::new())).collect(),
            per_shard: max.div_ceil(n).max(1),
        }
    }

    /// Adds `hits` to `key`'s counter for the window ending at `expires`,
    /// or takes them off it when `negative`, returning the new count.
    fn add(&self, key: String, expires: u64, now: u64, hits: u64, negative: bool) -> u64 {
        use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
        let h = BuildHasherDefault::<DefaultHasher>::default().hash_one(&key) as usize;
        let mut s = self.shards[h % self.shards.len()]
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if s.len() >= self.per_shard && !s.contains_key(&key) {
            s.retain(|_, (exp, _)| *exp > now);
            if s.len() >= self.per_shard
                && let Some(k) = s.keys().next().cloned()
            {
                // Full of live windows: the oldest key in iteration order
                // goes, which only ever lets a request through.
                s.remove(&k);
            }
        }
        let e = s.entry(key).or_insert((expires, 0));
        if e.0 != expires {
            *e = (expires, 0);
        }
        e.1 = if negative {
            e.1.saturating_sub(hits)
        } else {
            e.1.saturating_add(hits)
        };
        e.1
    }
}

/// The service: a config and its counters.
pub struct DescriptorLimiter {
    config: DescriptorConfig,
    counters: Counters,
}

impl fmt::Debug for DescriptorLimiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DescriptorLimiter")
            .field("domains", &self.config.domains.len())
            .finish()
    }
}

impl DescriptorLimiter {
    /// A limiter over `config` holding at most `max_counters` windows.
    pub fn new(config: DescriptorConfig, max_counters: usize) -> Self {
        DescriptorLimiter {
            config,
            counters: Counters::new(max_counters),
        }
    }

    pub fn config(&self) -> &DescriptorConfig {
        &self.config
    }

    /// The service's `ShouldRateLimit`: every descriptor of a request in
    /// `domain`, each adding `hits` (the request's `hits_addend`, 1 when
    /// it sets none) at `now_secs`, Unix time.
    pub fn should_rate_limit(
        &self,
        domain: &str,
        descriptors: &[&[(&str, &str)]],
        hits: u64,
        now_secs: u64,
    ) -> Response {
        let queries: Vec<Query<'_>> = descriptors
            .iter()
            .map(|entries| Query {
                entries,
                hits,
                negative: false,
                limit_override: None,
            })
            .collect();
        self.decide(domain, &queries, now_secs)
    }

    /// `ShouldRateLimit` with everything a descriptor of the v3 API can
    /// carry: its own hits (0 only checks), hits given back instead of
    /// taken, and a limit override from the caller.
    ///
    /// A descriptor with an override is limited by it alone, as the
    /// service's `GetLimit` does: never in shadow mode, never replaced,
    /// whatever the tree says, and counted on the same counter as the tree
    /// limit for the same entries. Negative hits take from the counter,
    /// never below zero.
    pub fn decide(&self, domain: &str, queries: &[Query<'_>], now_secs: u64) -> Response {
        let found: Vec<Option<Found<'_>>> = queries
            .iter()
            .map(|q| match q.limit_override {
                Some(o) => Some((
                    Cow::Owned(o.to_limit(domain, q.entries)),
                    vec![None; q.entries.len()],
                )),
                None => self
                    .config
                    .find(domain, q.entries)
                    .map(|(l, shared)| (Cow::Borrowed(l), shared)),
            })
            .collect();
        let replaced: Vec<&str> = found
            .iter()
            .flatten()
            .flat_map(|(l, _)| l.replaces.iter().map(String::as_str))
            .collect();
        let mut over_limit = false;
        let mut statuses = Vec::with_capacity(queries.len());
        for (q, f) in queries.iter().zip(&found) {
            let Some((limit, shared)) = f
                .as_ref()
                .filter(|(l, _)| l.name.as_deref().is_none_or(|n| !replaced.contains(&n)))
            else {
                statuses.push(DescriptorStatus {
                    over_limit: false,
                    limit: None,
                    limit_remaining: 0,
                    reset_secs: 0,
                });
                continue;
            };
            let Some(unit) = limit.unit.filter(|_| !limit.unlimited) else {
                statuses.push(DescriptorStatus {
                    over_limit: false,
                    limit: Some(limit.as_ref().clone()),
                    limit_remaining: u64::MAX,
                    reset_secs: 0,
                });
                continue;
            };
            let len = unit.secs() * u64::from(limit.unit_multiplier.max(1));
            let start = now_secs - now_secs % len;
            let mut key = String::with_capacity(64);
            key.push_str(domain);
            for ((k, v), share) in q.entries.iter().zip(shared) {
                key.push('_');
                key.push_str(k);
                key.push('_');
                key.push_str(share.unwrap_or(v));
            }
            key.push('_');
            key.push_str(&start.to_string());
            let count = self
                .counters
                .add(key, start + len, now_secs, q.hits, q.negative);
            let over = count > limit.requests_per_unit;
            if over && !limit.shadow_mode {
                over_limit = true;
            }
            statuses.push(DescriptorStatus {
                over_limit: over && !limit.shadow_mode,
                limit: Some(limit.as_ref().clone()),
                limit_remaining: limit.requests_per_unit.saturating_sub(count),
                reset_secs: start + len - now_secs,
            });
        }
        Response {
            over_limit,
            statuses,
        }
    }
}

/// A descriptor's limit, and per entry the shared wildcard pattern that
/// keys its counter instead of the value, if any.
type Found<'a> = (Cow<'a, RateLimit>, Vec<Option<&'a str>>);

/// One descriptor of a request, for [`DescriptorLimiter::decide`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Query<'a> {
    /// The descriptor's entries, key and value.
    pub entries: &'a [(&'a str, &'a str)],
    /// Hits to add; 0 checks without counting.
    pub hits: u64,
    /// Take `hits` off the counter instead, giving back budget.
    pub negative: bool,
    /// The caller's limit, used instead of the configured one.
    pub limit_override: Option<LimitOverride>,
}

/// A limit a request brings for one of its descriptors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitOverride {
    pub requests_per_unit: u64,
    pub unit: Unit,
    /// Units per window, 1 when the caller sets none.
    pub unit_multiplier: u32,
}

impl LimitOverride {
    /// The limit, named as the service names an override's stats:
    /// `domain.key1_value1.key2`, an entry with no value by its key.
    fn to_limit(self, domain: &str, entries: &[(&str, &str)]) -> RateLimit {
        let mut full_key = domain.to_owned();
        for (k, v) in entries {
            full_key.push('.');
            full_key.push_str(k);
            if !v.is_empty() {
                full_key.push('_');
                full_key.push_str(v);
            }
        }
        RateLimit {
            full_key,
            name: None,
            unit: Some(self.unit),
            unit_multiplier: self.unit_multiplier.max(1),
            requests_per_unit: self.requests_per_unit,
            unlimited: false,
            shadow_mode: false,
            replaces: Vec::new(),
            detailed_metric: false,
            share_threshold: false,
        }
    }
}

#[cfg(test)]
mod tests;
