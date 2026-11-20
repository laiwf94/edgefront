//! The limiter layer in the proxy pipeline: HTTP rules attached to routes
//! as an [`edgefront_core::filter::Filter`].
//!
//! [`HttpLimiter::route_filters`] gives each route a filter holding the
//! rules that apply to it, in config order, at most [`MAX_RULES_PER_ROUTE`].
//! A route without rules has no filter and pays nothing. For each request
//! on a route with rules, in the request phase, before an upstream is
//! picked:
//!
//! 1. each rule whose [`Match`] fits the request and whose
//!    [`KeySpec`] finds every field takes one token for the request's key
//!    from the rule's own [`Table`];
//! 2. a refused take does what the rule's [`Action`] says: **block**
//!    answers at once with the rule's refusal (429 by default) and gives
//!    back the tokens the earlier rules took, so a request refused by one
//!    rule spends nothing in another; **delay** books the token and holds
//!    the request until it is due, if that is within `max_delay`, and
//!    blocks otherwise; **log** lets the request through and counts it as
//!    one the rule would have refused;
//! 3. a request every rule admitted goes upstream with a settlement that
//!    the deliver phase completes: the tokens of rules counting only some
//!    statuses go back for the statuses they exclude
//!    ([`super::response_count`]), and the `RateLimit-Policy` and
//!    `RateLimit` headers ([`super::headers`]) go on the response.
//!
//! Keys are built on the stack ([`super::key`]); the request phase
//! allocates only its boxed future, the settlement and the header values.
//! Every decision reads the clock once, so all of a request's rules see
//! one instant.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use edgefront_core::body::{Body, full};
use edgefront_core::filter::{
    BoxFuture, Decision, Delivery, Exchange, Filter, RequestHead, RouteFilters, Upstream,
};
use edgefront_proxy::path::RequestTarget;
use http::header::{CONTENT_TYPE, RETRY_AFTER};
use http::{HeaderValue, Method, Response, StatusCode};

use super::headers::{self, RATELIMIT, RATELIMIT_POLICY};
use super::key::{Key, KeySpec, View, host_part, under_prefix};
use super::response_count::{Settle, StatusSet, settle};
use crate::clock::Clock;
use crate::gcra::{Error, Rate};
use crate::table::{FullPolicy, Table, TableConfig};

/// The most rules one route may carry.
pub const MAX_RULES_PER_ROUTE: usize = 16;

/// Which requests a rule applies to. Every condition given must hold; an
/// empty match takes every request on the rule's routes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Match {
    /// Lowercase, without port or trailing dot.
    pub host: Option<String>,
    /// Matched against the cleaned path as [`super::key::Selector::PathPrefix`] is.
    pub path_prefix: Option<String>,
    /// The cleaned path, exactly.
    pub path: Option<String>,
    /// Empty: any method.
    pub methods: Vec<Method>,
}

impl Match {
    #[inline]
    pub fn matches(&self, v: &View<'_>) -> bool {
        if !self.methods.is_empty() && !self.methods.contains(v.method) {
            return false;
        }
        if let Some(p) = &self.path
            && v.path != p.as_bytes()
        {
            return false;
        }
        if let Some(p) = &self.path_prefix
            && !under_prefix(v.path, p.as_bytes())
        {
            return false;
        }
        if let Some(h) = &self.host {
            match v.host() {
                Some(got) if got.eq_ignore_ascii_case(h.as_bytes()) => {}
                _ => return false,
            }
        }
        true
    }
}

/// What a rule does with a request over its limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Answer with the rule's refusal.
    Block,
    /// Hold the request until its token is due, when that is within the
    /// duration; block otherwise.
    Delay(Duration),
    /// Let it through, counting it and logging the rule's name.
    Log,
}

impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Action::Block => "block",
            Action::Delay(_) => "delay",
            Action::Log => "log",
        }
    }
}

/// The response a blocked request gets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub status: StatusCode,
    pub body: String,
    pub content_type: String,
}

impl Default for Refusal {
    fn default() -> Self {
        Refusal {
            status: StatusCode::TOO_MANY_REQUESTS,
            body: "too many requests\n".into(),
            content_type: "text/plain; charset=utf-8".into(),
        }
    }
}

/// One HTTP limit rule, validated.
#[derive(Clone, Debug, PartialEq)]
pub struct HttpRule {
    /// Letters, digits, `_` and `-`: it goes into headers and logs as is.
    pub name: String,
    /// Route indexes, in the proxy configuration's order; `None` for all.
    pub routes: Option<Vec<usize>>,
    pub matches: Match,
    pub key: KeySpec,
    pub rate: Rate,
    /// `None`: every response counts.
    pub count: Option<StatusSet>,
    pub action: Action,
    pub table_size: u32,
    pub table_full: FullPolicy,
    /// Whether responses carry this rule's RateLimit headers.
    pub headers: bool,
    pub refusal: Refusal,
}

/// A rule's counters.
#[derive(Debug, Default)]
struct Counters {
    matched: AtomicU64,
    allowed: AtomicU64,
    refused: AtomicU64,
    delayed: AtomicU64,
    logged: AtomicU64,
    refunded: AtomicU64,
    table_full: AtomicU64,
}

fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// A snapshot of one rule's counters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleStats {
    pub name: String,
    /// Requests the rule's match and key applied to.
    pub matched: u64,
    /// Admitted by this rule (including delayed ones once due).
    pub allowed: u64,
    /// Answered with the refusal.
    pub refused: u64,
    /// Held until their token was due.
    pub delayed: u64,
    /// Over the limit under the log action, and let through.
    pub logged: u64,
    /// Tokens given back for statuses the rule does not count.
    pub refunded: u64,
    /// Requests whose key found no room in the table.
    pub table_full: u64,
    /// Keys the rule's table holds now.
    pub keys: u64,
}

struct Rule {
    spec: HttpRule,
    table: Table,
    policy_item: String,
    counters: Counters,
}

/// Every HTTP rule of a config, with its table, shared by the route
/// filters.
pub struct HttpLimiter {
    rules: Vec<Rule>,
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for HttpLimiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpLimiter")
            .field("rules", &self.rules.len())
            .finish()
    }
}

impl HttpLimiter {
    /// Builds every rule's table. Fails on a table that cannot be made.
    pub fn new(rules: Vec<HttpRule>, clock: Arc<dyn Clock>) -> Result<Self, String> {
        let mut built = Vec::with_capacity(rules.len());
        for spec in rules {
            let mut cfg = TableConfig::new(spec.table_size, spec.rate);
            cfg.full_policy = spec.table_full;
            let table = Table::new(&cfg).map_err(|e| format!("http limit {}: {e}", spec.name))?;
            built.push(Rule {
                policy_item: headers::policy_item(&spec.name, &spec.rate),
                spec,
                table,
                counters: Counters::default(),
            });
        }
        Ok(HttpLimiter {
            rules: built,
            clock,
        })
    }

    /// A filter for each of `routes` routes that has rules. A rule naming
    /// a route past the last, or a route with more than
    /// [`MAX_RULES_PER_ROUTE`] rules, is refused.
    pub fn route_filters(self: &Arc<Self>, routes: usize) -> Result<RouteFilters, String> {
        let mut per_route = vec![Vec::new(); routes];
        for (ri, rule) in self.rules.iter().enumerate() {
            match &rule.spec.routes {
                None => per_route.iter_mut().for_each(|r| r.push(ri)),
                Some(list) => {
                    for &route in list {
                        let Some(r) = per_route.get_mut(route) else {
                            return Err(format!(
                                "http limit {}: route {route} does not exist; the proxy has {routes} routes",
                                rule.spec.name
                            ));
                        };
                        r.push(ri);
                    }
                }
            }
        }
        let mut filters = RouteFilters::new(routes);
        for (route, rules) in per_route.into_iter().enumerate() {
            if rules.is_empty() {
                continue;
            }
            if rules.len() > MAX_RULES_PER_ROUTE {
                return Err(format!(
                    "route {route} has {} http limits; at most {MAX_RULES_PER_ROUTE} may apply to one route",
                    rules.len()
                ));
            }
            filters.set(
                route,
                Arc::new(RouteLimits {
                    limiter: Arc::clone(self),
                    rules,
                }),
            );
        }
        Ok(filters)
    }

    /// Every rule's counters, in config order.
    pub fn stats(&self) -> Vec<RuleStats> {
        self.rules
            .iter()
            .map(|r| {
                let c = &r.counters;
                let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
                RuleStats {
                    name: r.spec.name.clone(),
                    matched: get(&c.matched),
                    allowed: get(&c.allowed),
                    refused: get(&c.refused),
                    delayed: get(&c.delayed),
                    logged: get(&c.logged),
                    refunded: get(&c.refunded),
                    table_full: get(&c.table_full),
                    keys: r.table.stats().entries,
                }
            })
            .collect()
    }

    /// The rules, in config order.
    pub fn rules(&self) -> impl Iterator<Item = &HttpRule> {
        self.rules.iter().map(|r| &r.spec)
    }

    /// Runs one request's head through `rules` (indexes into the
    /// limiter's rules), as a route filter does: the decision without the
    /// filter plumbing, for the benchmark and tests.
    pub async fn decide(
        self: &Arc<Self>,
        rules: &[usize],
        method: &Method,
        uri: &http::Uri,
        headers: &http::HeaderMap,
    ) -> Decision {
        decide(self, rules, method, uri, headers).await
    }
}

/// A token one rule took for a request.
#[derive(Clone, Copy, Debug)]
struct Charge {
    rule: usize,
    key: Key,
    remaining: u64,
    reset_ns: u64,
}

struct RouteLimits {
    limiter: Arc<HttpLimiter>,
    rules: Vec<usize>,
}

impl Filter for RouteLimits {
    fn request<'a>(&'a self, req: RequestHead<'a>) -> BoxFuture<'a, Decision> {
        let RequestHead {
            method,
            uri,
            headers,
            ..
        } = req;
        Box::pin(decide(&self.limiter, &self.rules, method, uri, headers))
    }
}

async fn decide(
    limiter: &Arc<HttpLimiter>,
    rules: &[usize],
    method: &Method,
    uri: &http::Uri,
    headers: &http::HeaderMap,
) -> Decision {
    let target = uri.path_and_query().map_or("/", |p| p.as_str());
    let parsed = RequestTarget::parse(target.as_bytes()).ok();
    let path = parsed.as_ref().map_or(&b"/"[..], |t| t.clean_path());
    let view = View {
        method,
        uri,
        headers,
        path,
    };
    let now = limiter.clock.now_ns();
    let mut charges: Vec<Charge> = Vec::new();
    let mut wait_ns = 0u64;
    for &ri in rules {
        let rule = &limiter.rules[ri];
        if !rule.spec.matches.matches(&view) {
            continue;
        }
        let Some(key) = rule.spec.key.key(&view) else {
            continue;
        };
        bump(&rule.counters.matched);
        let retry_ns = match rule.table.take_reset(key.as_bytes(), now, 1) {
            Ok((d, reset_ns)) if d.allowed => {
                bump(&rule.counters.allowed);
                charges.push(Charge {
                    rule: ri,
                    key,
                    remaining: d.remaining,
                    reset_ns,
                });
                continue;
            }
            Ok((d, _)) => d.retry_after_ns,
            Err(Error::TableFull) => {
                // Only under `table_full = "deny"`: the key is new and
                // there is no room for it, so it is refused as over the
                // limit until an entry goes idle.
                bump(&rule.counters.table_full);
                interval_ns(&rule.spec.rate)
            }
            Err(e) => {
                // Keys are at most KEY_MAX bytes and the cost of 1 fits
                // every burst, so this does not happen; let the request
                // through rather than refuse on a bug.
                tracing::error!(rule = %rule.spec.name, error = %e, "http limit decision failed");
                continue;
            }
        };
        match rule.spec.action {
            Action::Log => {
                bump(&rule.counters.logged);
                tracing::info!(
                    rule = %rule.spec.name,
                    retry_after_ms = retry_ns.div_ceil(1_000_000),
                    "http limit would refuse"
                );
                continue;
            }
            Action::Delay(max) if u128::from(retry_ns) <= max.as_nanos() => {
                if let Ok(b) = rule.table.book(key.as_bytes(), now, 1)
                    && b.allowed
                {
                    if u128::from(b.retry_after_ns) <= max.as_nanos() {
                        bump(&rule.counters.delayed);
                        bump(&rule.counters.allowed);
                        wait_ns = wait_ns.max(b.retry_after_ns);
                        charges.push(Charge {
                            rule: ri,
                            key,
                            remaining: 0,
                            reset_ns: b.retry_after_ns,
                        });
                        continue;
                    }
                    // Others booked first: the wait grew past the limit.
                    rule.table.refund(key.as_bytes(), now, 1);
                }
            }
            _ => {}
        }
        // Refused: no other rule keeps what it took for this request.
        bump(&rule.counters.refused);
        for c in &charges {
            limiter.rules[c.rule].table.refund(c.key.as_bytes(), now, 1);
        }
        return Decision::Respond(refusal(rule, retry_ns));
    }
    if wait_ns > 0 {
        tokio::time::sleep(Duration::from_nanos(wait_ns)).await;
    }
    let needed = charges.iter().any(|c| {
        let spec = &limiter.rules[c.rule].spec;
        spec.headers || spec.count.is_some()
    });
    if !needed {
        return Decision::Forward(None);
    }
    Decision::Forward(Some(Box::new(Settlement {
        limiter: Arc::clone(limiter),
        charges,
    })))
}

/// The nanoseconds one token takes to refill, rounded up.
fn interval_ns(rate: &Rate) -> u64 {
    u64::try_from((u128::from(rate.period_ns)).div_ceil(u128::from(rate.tokens.max(1))))
        .unwrap_or(u64::MAX)
}

fn refusal(rule: &Rule, retry_ns: u64) -> Response<Body> {
    let r = &rule.spec.refusal;
    let mut resp = Response::new(full(r.body.clone()));
    *resp.status_mut() = r.status;
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&r.content_type) {
        h.insert(CONTENT_TYPE, v);
    }
    h.insert(RETRY_AFTER, headers::retry_after(retry_ns));
    if rule.spec.headers {
        h.insert(
            RATELIMIT_POLICY,
            headers::list(std::slice::from_ref(&rule.policy_item)),
        );
        h.insert(
            RATELIMIT,
            headers::list(&[headers::refused_item(&rule.spec.name, retry_ns)]),
        );
    }
    resp
}

/// The deliver phase's half of a request: the tokens its rules took.
struct Settlement {
    limiter: Arc<HttpLimiter>,
    charges: Vec<Charge>,
}

impl Exchange for Settlement {
    fn upstream_response(&mut self, resp: Response<Body>) -> BoxFuture<'_, Upstream> {
        Box::pin(async move { Upstream::Respond(resp) })
    }

    fn deliver(self: Box<Self>, mut resp: Response<Body>, _from: Delivery) -> Response<Body> {
        let status = resp.status().as_u16();
        let mut now = None;
        let mut policies = Vec::new();
        let mut limits = Vec::new();
        for c in &self.charges {
            let rule = &self.limiter.rules[c.rule];
            let (mut remaining, mut reset_ns) = (c.remaining, c.reset_ns);
            if settle(rule.spec.count.as_ref(), status) == Settle::Refund {
                let t = *now.get_or_insert_with(|| self.limiter.clock.now_ns());
                rule.table.refund(c.key.as_bytes(), t, 1);
                bump(&rule.counters.refunded);
                remaining = (remaining + 1).min(rule.spec.rate.burst);
                reset_ns = reset_ns.saturating_sub(interval_ns(&rule.spec.rate));
            }
            if rule.spec.headers {
                policies.push(rule.policy_item.clone());
                limits.push(headers::limit_item(
                    &rule.spec.name,
                    &rule.spec.rate,
                    remaining,
                    reset_ns,
                ));
            }
        }
        if !policies.is_empty() {
            let h = resp.headers_mut();
            h.append(RATELIMIT_POLICY, headers::list(&policies));
            h.append(RATELIMIT, headers::list(&limits));
        }
        resp
    }
}

/// [`host_part`] lowercased, as [`Match::host`] holds it.
pub fn normal_host(host: &str) -> String {
    String::from_utf8_lossy(host_part(host.as_bytes())).to_ascii_lowercase()
}

#[cfg(test)]
mod tests;
