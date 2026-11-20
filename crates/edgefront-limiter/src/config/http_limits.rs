//! The `[[http_limits]]` section: HTTP rate limiting rules for the
//! proxy's routes, built into [`HttpRule`]s for
//! [`crate::http::HttpLimiter`].
//!
//! ```toml
//! [[http_limits]]
//! name = "login"                          # letters, digits, _ and -
//! routes = [0]                            # proxy route indexes; default every route
//! rate = "5/min burst 5"                  # a RateSpec token bucket
//! key = ["path", "header:x-forwarded-for"]
//! count = [401, 403]                      # codes and "4xx" classes; default every response
//! action = "block"                        # block, delay or log
//! max_delay = "500ms"                     # delay only
//! table_size = 100000                     # keys tracked, default 65536
//! table_full = "evict"                    # or "deny"
//! headers = true                          # RateLimit-Policy and RateLimit
//! [http_limits.match]
//! host = "app.example.com"
//! path_prefix = "/login"
//! methods = ["POST"]
//! cost = "request_bytes"                 # or a number, "graphql", "response_header:x-cost"
//! cost_missing = 4096                     # the cost when the request says nothing
//! [http_limits.response]
//! status = 429
//! body = "slow down\n"
//! content_type = "text/plain"
//! ```
//!
//! Every mistake names its field: `http_limits[1].key[0]: unknown key
//! selector "ip"; ...`. The section is read on its own, like
//! `[address_policy]`, so a file may hold only it (`edgefront proxy
//! --limits`) or sit beside a limiter section.
//!
//! [`lint`] adds the warnings `edgefront check` prints for rules that are
//! valid but cannot do what they say:
//!
//! | class | finding |
//! |---|---|
//! | `http-key-never-matches` | a `path_prefix` key selector or the match's `path` lies outside the match's `path_prefix`, so no request has the key and the rule never applies |
//! | `http-key-host-header` | a `header:host` selector or match: HTTP/2 requests carry `:authority`, not `Host`, so those requests get no key; the `host` selector reads both |
//! | `http-count-own-status` | `count` includes the refusal's own status, which the rule's refusals never reach (they are answered before any upstream), so the code counts only upstream answers |
//! | `http-delay-unused` | `max_delay` is set on a rule whose action is not `delay` |
//! | `http-shadow-shadowed` | a shadow rule follows an enforcing rule at least as strict on the same requests and keys, so it never sees the requests that one refuses |

use std::time::Duration;

use serde::Deserialize;

use crate::cost::Cost;
use crate::http::key::{KeySpec, Selector};
use crate::http::layer::{Action, HttpRule, Match, Refusal, normal_host};
use crate::http::response_count::StatusSet;
use crate::lint::Class;
use crate::rate::{Plan, RateSpec};
use crate::table::{FullPolicy, MAX_ENTRIES};

/// The default `table_size`.
pub const DEFAULT_TABLE_SIZE: u32 = 65536;
/// The longest `max_delay`.
pub const MAX_DELAY: Duration = Duration::from_secs(60);
/// The longest refusal body.
pub const BODY_MAX: usize = 4096;

/// A status code or class as written: `401` or `"4xx"`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum StatusItem {
    Code(u64),
    Text(String),
}

/// The match as written.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub methods: Vec<String>,
    /// Refused with a pointer to `key`: matching on a header is not
    /// supported yet, keying on one is.
    #[serde(default)]
    pub header: Option<serde::de::IgnoredAny>,
}

/// The refusal as written.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResponseConfig {
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
}

/// One rule as written. Unknown keys are refused.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HttpLimitConfig {
    pub name: String,
    #[serde(default)]
    pub routes: Option<Vec<usize>>,
    #[serde(default, rename = "match")]
    pub matches: MatchConfig,
    #[serde(default)]
    pub key: Vec<String>,
    pub rate: String,
    #[serde(default)]
    pub count: Option<Vec<StatusItem>>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub max_delay: Option<String>,
    #[serde(default)]
    pub table_size: Option<u32>,
    #[serde(default)]
    pub table_full: Option<String>,
    #[serde(default)]
    pub headers: Option<bool>,
    #[serde(default)]
    pub response: Option<ResponseConfig>,
    /// A number, `"request_bytes"`, `"graphql"` or
    /// `"response_header:NAME"`; 1 by default.
    #[serde(default)]
    pub cost: Option<StatusItem>,
    /// What a request costs when the cost's source says nothing (no
    /// `Content-Length`, no query, no header); 1 by default.
    #[serde(default)]
    pub cost_missing: Option<u64>,
    #[serde(default)]
    pub penalty: Option<PenaltyToml>,
    /// `enforce` (default), `shadow` or `off`.
    #[serde(default)]
    pub mode: Option<String>,
}

/// A rule's `[http_limits.penalty]` as written.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PenaltyToml {
    /// Refused in this many consecutive windows boxes the key; 3 by default.
    #[serde(default)]
    pub windows: Option<u32>,
    /// The window, 1s by default.
    #[serde(default)]
    pub window: Option<String>,
    /// The first mitigation timeout.
    pub timeout: String,
    /// The longest a repeat offence doubles it to; 10 times `timeout` by default.
    #[serde(default)]
    pub max_timeout: Option<String>,
    /// Clean this long after release, a key starts over; 10m by default.
    #[serde(default)]
    pub forget: Option<String>,
    /// Keys tracked, 65536 by default.
    #[serde(default)]
    pub capacity: Option<usize>,
    /// Boxed this many times running bans the key for `ban_for`.
    #[serde(default)]
    pub ban_after: Option<u32>,
    #[serde(default)]
    pub ban_for: Option<String>,
}

/// A document's `http_limits` array, ignoring every other section, so
/// the binary can read it from any TOML config with serde.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct HttpLimitsSection {
    #[serde(default)]
    pub http_limits: Vec<HttpLimitConfig>,
}

fn duration(text: &str) -> Result<Duration, String> {
    super::parse_duration(text.as_bytes())
        .map(Duration::from_nanos)
        .ok_or_else(|| format!("expected a duration like 250ms or 2s, got {text:?}"))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl HttpLimitConfig {
    /// The rule, or the first mistake as `field: reason`.
    pub fn build(&self) -> Result<HttpRule, String> {
        if !valid_name(&self.name) {
            return Err(format!(
                "name: {:?} must be 1 to 63 letters, digits, _ or -",
                self.name
            ));
        }
        let spec = RateSpec::parse(&self.rate).map_err(|e| format!("rate: {e}"))?;
        let rate = match spec.plan() {
            Plan::Bucket(r) => r,
            Plan::Quota { .. } => {
                return Err(format!(
                    "rate: {:?} is a quota window; HTTP limits take a token bucket, a period under an hour or a burst",
                    self.rate
                ));
            }
        };
        let mut selectors = Vec::with_capacity(self.key.len());
        for (i, k) in self.key.iter().enumerate() {
            let s = Selector::parse(k).map_err(|e| format!("key[{i}]: {e}"))?;
            if selectors.contains(&s) {
                return Err(format!("key[{i}]: {k:?} is listed twice"));
            }
            selectors.push(s);
        }
        let m = &self.matches;
        if m.header.is_some() {
            return Err(
                "match.header: matching on headers is not supported; key on one with key = [\"header:NAME\"]"
                    .into(),
            );
        }
        let mut methods = Vec::new();
        for (i, name) in m.methods.iter().enumerate() {
            let method = http::Method::from_bytes(name.to_ascii_uppercase().as_bytes())
                .map_err(|_| format!("match.methods[{i}]: {name:?} is not a method"))?;
            if methods.contains(&method) {
                return Err(format!("match.methods[{i}]: {name:?} is listed twice"));
            }
            methods.push(method);
        }
        for (field, p) in [("path_prefix", &m.path_prefix), ("path", &m.path)] {
            if let Some(p) = p {
                Selector::parse(&format!("path_prefix:{p}")).map_err(|e| {
                    format!("match.{field}: {}", e.trim_start_matches("path_prefix "))
                })?;
            }
        }
        if let Some(h) = &m.host
            && (h.is_empty() || h.bytes().any(|b| b.is_ascii_whitespace() || b == b'/'))
        {
            return Err(format!("match.host: {h:?} is not a host name"));
        }
        let count = match &self.count {
            None => None,
            Some(items) => {
                let texts: Vec<String> = items
                    .iter()
                    .map(|i| match i {
                        StatusItem::Code(c) => c.to_string(),
                        StatusItem::Text(t) => t.clone(),
                    })
                    .collect();
                Some(
                    StatusSet::parse(texts.iter().map(String::as_str))
                        .map_err(|e| format!("count: {e}"))?,
                )
            }
        };
        let action = match self.action.as_deref().unwrap_or("block") {
            "block" => Action::Block,
            "log" => Action::Log,
            "delay" => {
                let max = match &self.max_delay {
                    Some(t) => duration(t).map_err(|e| format!("max_delay: {e}"))?,
                    None => {
                        return Err("max_delay: delay needs a max_delay, such as \"500ms\"".into());
                    }
                };
                if max.is_zero() || max > MAX_DELAY {
                    return Err("max_delay: must be from 1ms to 60s".into());
                }
                Action::Delay(max)
            }
            other => return Err(format!("action: {other:?} is not block, delay or log")),
        };
        if let Some(t) = &self.max_delay
            && !matches!(action, Action::Delay(_))
        {
            duration(t).map_err(|e| format!("max_delay: {e}"))?;
        }
        let table_size = self.table_size.unwrap_or(DEFAULT_TABLE_SIZE);
        if table_size == 0 || table_size > MAX_ENTRIES {
            return Err(format!("table_size: must be between 1 and {MAX_ENTRIES}"));
        }
        let table_full = match self.table_full.as_deref().unwrap_or("evict") {
            "evict" => FullPolicy::Evict,
            "deny" => FullPolicy::Deny,
            other => return Err(format!("table_full: {other:?} is not evict or deny")),
        };
        let mut refusal = Refusal::default();
        if let Some(r) = &self.response {
            if let Some(code) = r.status {
                refusal.status = http::StatusCode::from_u16(code)
                    .ok()
                    .filter(|s| s.is_client_error() || s.is_server_error())
                    .ok_or_else(|| {
                        format!("response.status: {code} is not a status from 400 to 599")
                    })?;
            }
            if let Some(b) = &r.body {
                if b.len() > BODY_MAX {
                    return Err(format!("response.body: longer than {BODY_MAX} bytes"));
                }
                refusal.body = b.clone();
            }
            if let Some(ct) = &r.content_type {
                if http::HeaderValue::from_str(ct).is_err() || ct.is_empty() {
                    return Err(format!(
                        "response.content_type: {ct:?} is not a header value"
                    ));
                }
                refusal.content_type = ct.clone();
            }
        }
        let missing = self.cost_missing.unwrap_or(1);
        let cost = match &self.cost {
            None => Ok(Cost::Fixed(1)),
            Some(StatusItem::Code(n)) => Cost::parse(&n.to_string(), missing),
            Some(StatusItem::Text(t)) => Cost::parse(t, missing),
        }
        .map_err(|e| format!("cost: {e}"))?;
        if let Cost::Fixed(n) = cost
            && n > rate.burst
        {
            return Err(format!(
                "cost: {n} is more than the burst of {}, so no request would ever be admitted",
                rate.burst
            ));
        }
        if self.cost_missing.is_some() && matches!(cost, Cost::Fixed(_)) {
            return Err(
                "cost_missing: only a cost read from the request or response has a missing case"
                    .into(),
            );
        }
        let penalty = match &self.penalty {
            None => None,
            Some(p) => {
                let d = |field: &str,
                         v: &Option<String>,
                         default: Duration|
                 -> Result<Duration, String> {
                    v.as_deref()
                        .map(duration)
                        .transpose()
                        .map_err(|e| format!("penalty.{field}: {e}"))
                        .map(|o| o.unwrap_or(default))
                };
                let timeout = duration(&p.timeout).map_err(|e| format!("penalty.timeout: {e}"))?;
                let window = d("window", &p.window, Duration::from_secs(1))?;
                let max_timeout = d("max_timeout", &p.max_timeout, timeout * 10)?;
                let forget = d("forget", &p.forget, Duration::from_secs(600))?;
                if timeout.is_zero() || window.is_zero() {
                    return Err("penalty: timeout and window must be at least 1ms".into());
                }
                if max_timeout < timeout {
                    return Err("penalty.max_timeout: shorter than timeout".into());
                }
                if p.windows == Some(0) || p.ban_after == Some(0) {
                    return Err("penalty: windows and ban_after must be at least 1".into());
                }
                let ban_ns = match (&p.ban_after, &p.ban_for) {
                    (Some(_), Some(f)) => duration(f)
                        .map_err(|e| format!("penalty.ban_for: {e}"))?
                        .as_nanos() as u64,
                    (None, None) => 0,
                    _ => return Err("penalty: ban_after and ban_for go together".into()),
                };
                Some(crate::http::layer::Penalty {
                    boxing: crate::penalty::PenaltyConfig {
                        windows: p.windows.unwrap_or(3),
                        window_ns: window.as_nanos() as u64,
                        timeout_ns: timeout.as_nanos() as u64,
                        max_timeout_ns: max_timeout.as_nanos() as u64,
                        forget_ns: forget.as_nanos() as u64,
                        capacity: p.capacity.unwrap_or(65536).max(1),
                    },
                    ban_after: p.ban_after,
                    ban_ns,
                })
            }
        };
        let mode = match self.mode.as_deref() {
            None => crate::shadow::Mode::Enforce,
            Some(m) => crate::shadow::Mode::parse(m)
                .ok_or_else(|| format!("mode: {m:?} is not enforce, shadow or off"))?,
        };
        Ok(HttpRule {
            mode,
            penalty,
            cost,
            name: self.name.clone(),
            routes: self.routes.clone(),
            matches: Match {
                host: m.host.as_deref().map(normal_host),
                path_prefix: m.path_prefix.clone(),
                path: m.path.clone(),
                methods,
            },
            key: KeySpec::new(selectors),
            rate,
            count,
            action,
            table_size,
            table_full,
            headers: self.headers.unwrap_or(true),
            refusal,
        })
    }
}

/// Every rule of the section, or the first mistake as
/// `http_limits[i].field: reason`. Names must be unique.
pub fn build(rules: &[HttpLimitConfig]) -> Result<Vec<HttpRule>, String> {
    let mut out: Vec<HttpRule> = Vec::with_capacity(rules.len());
    for (i, r) in rules.iter().enumerate() {
        let rule = r.build().map_err(|e| format!("http_limits[{i}].{e}"))?;
        if out.iter().any(|o| o.name == rule.name) {
            return Err(format!(
                "http_limits[{i}].name: {:?} is used twice",
                rule.name
            ));
        }
        out.push(rule);
    }
    Ok(out)
}

/// One warning about a rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpLint {
    /// The rule's index in the section.
    pub rule: usize,
    pub name: String,
    pub class: Class,
    pub message: String,
}

impl std::fmt::Display for HttpLint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "warning[{}]: http limit {}: {}",
            self.class, self.name, self.message
        )
    }
}

/// The warnings for built `rules`, in rule order.
pub fn lint(rules: &[HttpRule]) -> Vec<HttpLint> {
    use crate::http::key::under_prefix;
    let mut out = Vec::new();
    for (i, r) in rules.iter().enumerate() {
        // A shadow rule after an enforcing one that is at least as strict
        // on the same requests and keys never sees what that one refuses,
        // so it undercounts what it would block.
        if r.mode == crate::shadow::Mode::Shadow
            && let Some(e) = rules[..i].iter().find(|e| {
                e.mode == crate::shadow::Mode::Enforce
                    && (e.routes.is_none() || e.routes == r.routes)
                    && (e.matches == Match::default() || e.matches == r.matches)
                    && e.key == r.key
                    && u128::from(e.rate.tokens) * u128::from(r.rate.period_ns)
                        <= u128::from(r.rate.tokens) * u128::from(e.rate.period_ns)
                    && e.rate.burst <= r.rate.burst
            })
        {
            out.push(HttpLint {
                rule: i,
                name: r.name.clone(),
                class: Class::HttpShadowShadowed,
                message: format!(
                    "shadow limit {} comes after {}, which enforces at least as tight a limit on the same requests and keys, so it never sees what {} refuses and its would-block count is low",
                    r.name, e.name, e.name
                ),
            });
        }
        let mut warn = |class, message: String| {
            out.push(HttpLint {
                rule: i,
                name: r.name.clone(),
                class,
                message,
            })
        };
        // Two prefixes overlap when one is under the other.
        let overlap = |a: &str, b: &str| {
            under_prefix(a.as_bytes(), b.as_bytes()) || under_prefix(b.as_bytes(), a.as_bytes())
        };
        if let Some(mp) = &r.matches.path_prefix {
            for s in r.key.selectors() {
                if let Selector::PathPrefix(kp) = s
                    && !overlap(kp, mp)
                {
                    warn(
                        Class::HttpKeyNeverMatches,
                        format!(
                            "key {} lies outside match.path_prefix {mp}, so no request has the key and the rule never applies",
                            s.text()
                        ),
                    );
                }
            }
            if let Some(p) = &r.matches.path
                && !under_prefix(p.as_bytes(), mp.as_bytes())
            {
                warn(
                    Class::HttpKeyNeverMatches,
                    format!(
                        "match.path {p} lies outside match.path_prefix {mp}, so the rule never applies"
                    ),
                );
            }
        }
        if let Some(p) = &r.matches.path {
            for s in r.key.selectors() {
                if let Selector::PathPrefix(kp) = s
                    && !under_prefix(p.as_bytes(), kp.as_bytes())
                {
                    warn(
                        Class::HttpKeyNeverMatches,
                        format!(
                            "key {} does not cover match.path {p}, so no request has the key and the rule never applies",
                            s.text()
                        ),
                    );
                }
            }
        }
        if r.key
            .selectors()
            .iter()
            .any(|s| matches!(s, Selector::Header(h) if h == http::header::HOST))
        {
            warn(
                Class::HttpKeyHostHeader,
                "key header:host finds nothing in HTTP/2 requests, which carry :authority; the host selector reads both".into(),
            );
        }
        if let Some(c) = &r.count
            && c.contains(r.refusal.status.as_u16())
        {
            warn(
                Class::HttpCountOwnStatus,
                format!(
                    "count includes {}, the rule's own refusal, which is answered before any upstream and never counted; only upstream {} responses are",
                    r.refusal.status.as_u16(),
                    r.refusal.status.as_u16()
                ),
            );
        }
    }
    out
}

/// [`lint`] for rules that also say whether `max_delay` was written,
/// which a built rule forgets: the configs and their built rules, in the
/// same order.
pub fn lint_configs(configs: &[HttpLimitConfig], rules: &[HttpRule]) -> Vec<HttpLint> {
    let mut out = lint(rules);
    for (i, (c, r)) in configs.iter().zip(rules).enumerate() {
        if c.max_delay.is_some() && !matches!(r.action, Action::Delay(_)) {
            out.push(HttpLint {
                rule: i,
                name: r.name.clone(),
                class: Class::HttpDelayUnused,
                message: format!(
                    "max_delay is set but the action is {}, so it never delays",
                    r.action.name()
                ),
            });
        }
    }
    out.sort_by_key(|l| (l.rule, l.class));
    out
}

#[cfg(test)]
mod tests;
