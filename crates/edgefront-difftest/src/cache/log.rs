//! The request log: one JSON object per line, and a seeded generator that
//! records a workload in it.
//!
//! ```text
//! {"t":0,"to":"proxy","method":"GET","path":"/static/3","headers":[["host","www.example.test"],["x-request-id","r0"]]}
//! {"t":4,"to":"admin","method":"POST","path":"/purge","headers":[["authorization","Bearer difftest-admin-token"]],"body":"{\"tags\":[\"news\"]}"}
//! {"t":9,"to":"origin","method":"POST","path":"/bump"}
//! ```
//!
//! `t` is the logical time in whole seconds since the log started, and
//! never goes back. `to` says where the request goes: a node's proxy
//! listener (the default), its admin listener, or the origin, where the
//! path names a scenario event (see [`super::origin`]). Headers keep their
//! order and may repeat. The body is text.

use serde::{Deserialize, Serialize};

use super::origin::{self, EVENTS, START_OF_CONTENT};
use crate::rng::Rng;

/// The admin token both nodes run with.
pub const ADMIN_TOKEN: &str = "difftest-admin-token";

/// The host most requests name.
pub const HOST: &str = "www.example.test";

/// Where a log entry goes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    #[default]
    Proxy,
    Admin,
    Origin,
}

/// One recorded request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub t: u64,
    #[serde(default)]
    pub to: Target,
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
}

impl Entry {
    /// The first value of header `name`, compared without case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn check(&self) -> Result<(), String> {
        if http::Method::from_bytes(self.method.as_bytes()).is_err() {
            return Err(format!("invalid method {:?}", self.method));
        }
        if !self.path.starts_with('/') || self.path.parse::<http::Uri>().is_err() {
            return Err(format!("invalid path {:?}", self.path));
        }
        for (name, value) in &self.headers {
            if http::HeaderName::from_bytes(name.as_bytes()).is_err() {
                return Err(format!("invalid header name {name:?}"));
            }
            if http::HeaderValue::from_str(value).is_err() {
                return Err(format!("invalid value for header {name}"));
            }
        }
        if self.to == Target::Origin && !EVENTS.contains(&self.path.as_str()) {
            return Err(format!(
                "origin entries name an event ({}), not {:?}",
                EVENTS.join(", "),
                self.path
            ));
        }
        Ok(())
    }
}

/// Reads a log, refusing the first line that is not a valid entry or whose
/// time goes back. Blank lines are skipped.
pub fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    let mut last = 0;
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: Entry =
            serde_json::from_str(line).map_err(|e| format!("line {}: {e}", n + 1))?;
        entry.check().map_err(|e| format!("line {}: {e}", n + 1))?;
        if entry.t < last {
            return Err(format!(
                "line {}: time goes back from {last} to {}",
                n + 1,
                entry.t
            ));
        }
        last = entry.t;
        entries.push(entry);
    }
    Ok(entries)
}

/// Writes entries one per line.
pub fn write(entries: &[Entry]) -> String {
    let mut out = String::new();
    for e in entries {
        out.push_str(&serde_json::to_string(e).expect("entries serialise"));
        out.push('\n');
    }
    out
}

/// How many resources each route family has, and how often it is asked
/// for (weights out of their sum).
const FAMILIES: [(&str, usize, usize); 16] = [
    ("static", 400, 35),
    ("page", 100, 15),
    ("news", 20, 8),
    ("heuristic", 50, 5),
    ("vary", 30, 6),
    ("lang", 20, 4),
    ("nostore", 20, 3),
    ("private", 20, 3),
    ("cookie", 20, 2),
    ("nocache", 30, 4),
    ("smax", 30, 3),
    ("expires", 30, 3),
    ("big", 10, 1),
    ("status", 10, 4),
    ("nowhere", 10, 1),
    ("healthz", 1, 1),
];

/// The sizes a `/static/{i}?size=N` request asks for.
const QUERY_SIZES: [usize; 5] = [0, 1, 100, 4096, 20_000];

const STATUSES: [u16; 8] = [404, 410, 301, 308, 500, 503, 204, 200];

fn weighted<'a>(rng: &mut Rng, items: &'a [(&'a str, usize, usize)]) -> (&'a str, usize) {
    let total: usize = items.iter().map(|i| i.2).sum();
    let mut r = rng.below(total);
    for &(name, count, weight) in items {
        if r < weight {
            return (name, count);
        }
        r -= weight;
    }
    unreachable!("r < total")
}

/// An index in `0..n` skewed towards small numbers, so a few resources
/// are hot and most are cold.
fn skewed(rng: &mut Rng, n: usize) -> usize {
    let bound = rng.below(n) + 1;
    rng.below(bound)
}

/// What the generator knows about the origin, to send conditionals that
/// sometimes match.
struct Scenario {
    version: u64,
    failing: bool,
}

fn http_date(t: u64) -> String {
    httpdate::fmt_http_date(std::time::UNIX_EPOCH + std::time::Duration::from_secs(t))
}

/// Generates `n` requests from `seed`, with admin purges and origin
/// events among them (they count towards `n`). The working set stays far
/// below the nodes' memory budget, so no node ever evicts: eviction order
/// depends on a per-process hash seed and is not part of the diff.
pub fn generate(seed: u64, n: usize) -> Vec<Entry> {
    let mut rng = Rng::new(seed);
    let mut t = 0u64;
    let mut scenario = Scenario {
        version: 0,
        failing: false,
    };
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        // About a second every six requests, and now and then a quiet
        // spell longer than the longest freshness lifetime: 50,000
        // requests cover about six logical hours.
        if rng.chance(0.0005) {
            t += rng.range(300, 900) as u64;
        } else if rng.chance(0.08) {
            t += rng.range(1, 3) as u64;
        }
        let roll = rng.below(1000);
        let entry = if roll < 3 {
            origin_event(&mut rng, &mut scenario, t)
        } else if scenario.failing && roll < 60 {
            // A failing origin recovers after twenty or so requests.
            scenario.failing = false;
            event(t, "/recover")
        } else if roll < 10 {
            admin(&mut rng, t)
        } else {
            proxy(&mut rng, &scenario, t, k)
        };
        out.push(entry);
    }
    out
}

fn event(t: u64, path: &str) -> Entry {
    Entry {
        t,
        to: Target::Origin,
        method: "POST".into(),
        path: path.into(),
        headers: Vec::new(),
        body: String::new(),
    }
}

fn origin_event(rng: &mut Rng, s: &mut Scenario, t: u64) -> Entry {
    if !s.failing && rng.chance(0.4) {
        s.failing = true;
        event(t, "/fail")
    } else {
        s.version += 1;
        event(t, "/bump")
    }
}

fn admin(rng: &mut Rng, t: u64) -> Entry {
    let auth = ("authorization".to_string(), format!("Bearer {ADMIN_TOKEN}"));
    let (headers, body, method, path) = match rng.below(12) {
        0 => (vec![], String::new(), "GET", "/healthz"),
        1 => (vec![], r#"{"tags":["news"]}"#.to_string(), "POST", "/purge"),
        2 => (
            vec![("authorization".into(), "Bearer wrong".into())],
            r#"{"tags":["news"]}"#.to_string(),
            "POST",
            "/purge",
        ),
        3 => (vec![auth], "not json".to_string(), "POST", "/purge"),
        4 => (vec![auth], r#"{"tags":[]}"#.to_string(), "POST", "/purge"),
        5 => (vec![auth], String::new(), "GET", "/purge"),
        6 => (
            vec![auth],
            r#"{"urls":["/page/1"]}"#.to_string(),
            "POST",
            "/purge",
        ),
        7 => (
            vec![auth],
            r#"{"tags":["static"]}"#.to_string(),
            "POST",
            "/purge",
        ),
        8 => {
            let i = skewed(rng, 100);
            let body = format!(r#"{{"urls":["http://{HOST}/page/{i}"]}}"#);
            (vec![auth], body, "POST", "/purge")
        }
        9 => {
            let i = skewed(rng, 400);
            let body = format!(r#"{{"tags":["s{i}","page"],"urls":["http://{HOST}/static/{i}"]}}"#);
            (vec![auth], body, "POST", "/purge")
        }
        10 => (
            vec![auth],
            r#"{"tags":["page"]}"#.to_string(),
            "POST",
            "/purge",
        ),
        _ => (
            vec![auth],
            r#"{"tags":["news"]}"#.to_string(),
            "POST",
            "/purge",
        ),
    };
    let mut headers = headers;
    if !body.is_empty() {
        headers.push(("content-type".into(), "application/json".into()));
    }
    Entry {
        t,
        to: Target::Admin,
        method: method.into(),
        path: path.into(),
        headers,
        body,
    }
}

fn proxy(rng: &mut Rng, s: &Scenario, t: u64, k: usize) -> Entry {
    let (family, count) = weighted(rng, &FAMILIES);
    let i = skewed(rng, count);
    let mut path = match family {
        "healthz" => "/healthz".to_string(),
        "status" => format!("/status/{}/{i}", STATUSES[i % STATUSES.len()]),
        f => format!("/{f}/{i}"),
    };
    let mut query = None;
    if family == "static" && rng.chance(0.08) {
        let size = rng.pick(&QUERY_SIZES);
        query = Some(format!("size={size}"));
        path = format!("{path}?size={size}");
    }
    let method = match rng.below(100) {
        0..88 => "GET",
        88..95 => "HEAD",
        95..98 => "POST",
        98 => "PUT",
        _ => "DELETE",
    };
    let mut h: Vec<(String, String)> = Vec::new();
    let host = match rng.below(100) {
        0..90 => HOST,
        90..97 => "static.example.test",
        _ => "WWW.Example.Test",
    };
    h.push(("host".into(), host.into()));
    if rng.chance(0.97) {
        h.push(("x-request-id".into(), format!("r{k}")));
    }
    let encodings = ["gzip", "br, gzip", "identity", "gzip, deflate"];
    if family == "vary" {
        if rng.chance(0.8) {
            h.push(("accept-encoding".into(), rng.pick(&encodings).into()));
        }
    } else if rng.chance(0.5) {
        h.push(("accept-encoding".into(), "gzip".into()));
    }
    if family == "lang" && rng.chance(0.8) {
        let langs = ["en", "fr", "de", "en-GB, en;q=0.8"];
        h.push(("accept-language".into(), rng.pick(&langs).into()));
    }
    if method == "GET" && family == "static" && rng.chance(0.08) {
        let ranges = [
            "bytes=0-99",
            "bytes=100-",
            "bytes=-50",
            "bytes=0-0",
            "bytes=999999-",
            "bytes=5-2",
            "bytes=0-9,20-29",
            "items=0-1",
        ];
        h.push(("range".into(), rng.pick(&ranges).into()));
    }
    if rng.chance(0.06) {
        let current = match family {
            "static" => {
                let size = origin::static_size(i as u64, query.as_deref());
                format!("\"s{i}-{size}\"")
            }
            "page" => format!("\"p{i}-v{}\"", s.version),
            "nocache" => format!("\"n{i}-v{}\"", s.version),
            _ => "\"none\"".to_string(),
        };
        let stale = format!("\"p{i}-v{}\"", s.version.saturating_sub(1));
        let value = match rng.below(4) {
            0 | 1 => current,
            2 => stale,
            _ => "*".to_string(),
        };
        h.push(("if-none-match".into(), value));
    }
    if rng.chance(0.03) {
        let at = START_OF_CONTENT + rng.below(40) as u64 * 3600;
        h.push(("if-modified-since".into(), http_date(at)));
    }
    if rng.chance(0.04) {
        let directives = [
            "no-cache",
            "max-age=0",
            "only-if-cached",
            "max-stale=100",
            "no-store",
            "max-age=5",
            "min-fresh=20",
            "max-stale",
        ];
        h.push(("cache-control".into(), rng.pick(&directives).into()));
    }
    if rng.chance(0.01) {
        h.push(("pragma".into(), "no-cache".into()));
    }
    if rng.chance(0.01) {
        h.push(("authorization".into(), "Bearer user-token".into()));
    }
    if rng.chance(0.01) {
        h.push(("cookie".into(), format!("session=c{i}")));
    }
    let body = if matches!(method, "POST" | "PUT") {
        h.push((
            "content-type".into(),
            "application/x-www-form-urlencoded".into(),
        ));
        format!("k=v{k}")
    } else {
        String::new()
    };
    Entry {
        t,
        to: Target::Proxy,
        method: method.into(),
        path,
        headers: h,
        body,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    #[test]
    fn generation_is_seeded() {
        assert_eq!(generate(7, 2000), generate(7, 2000));
        assert_ne!(generate(7, 2000), generate(8, 2000));
        assert_eq!(generate(7, 100), generate(7, 2000)[..100]);
    }

    #[test]
    fn generated_logs_round_trip_and_cover_the_routes() {
        let entries = generate(42, 20_000);
        let text = write(&entries);
        assert_eq!(parse(&text).unwrap(), entries);

        let mut targets = BTreeMap::new();
        let mut families = BTreeSet::new();
        let mut methods = BTreeSet::new();
        let mut headers = BTreeSet::new();
        let mut events = BTreeSet::new();
        for e in &entries {
            *targets.entry(format!("{:?}", e.to)).or_insert(0) += 1;
            match e.to {
                Target::Proxy => {
                    families.insert(e.path.split('/').nth(1).unwrap().to_string());
                    methods.insert(e.method.clone());
                    headers.extend(e.headers.iter().map(|(n, _)| n.clone()));
                }
                Target::Origin => {
                    events.insert(e.path.clone());
                }
                Target::Admin => {}
            }
        }
        assert_eq!(families.len(), FAMILIES.len());
        assert_eq!(methods.len(), 5, "{methods:?}");
        for h in [
            "range",
            "if-none-match",
            "if-modified-since",
            "cache-control",
            "pragma",
            "authorization",
            "cookie",
            "accept-language",
            "accept-encoding",
        ] {
            assert!(headers.contains(h), "{h}");
        }
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(targets["Admin"] > 50, "{targets:?}");
        // Time moves forward, past the longest freshness lifetime more
        // than once.
        assert!(entries.windows(2).all(|w| w[0].t <= w[1].t));
        assert!(entries.last().unwrap().t > 3_600);
    }

    #[test]
    fn an_origin_failure_always_recovers() {
        let entries = generate(3, 50_000);
        let mut failing_for = 0;
        let mut longest = 0;
        for e in &entries {
            match (e.to, e.path.as_str()) {
                (Target::Origin, "/fail") => failing_for = 1,
                (Target::Origin, "/recover") => failing_for = 0,
                _ if failing_for > 0 => failing_for += 1,
                _ => {}
            }
            longest = longest.max(failing_for);
        }
        assert!(longest < 400, "failed for {longest} requests");
    }

    #[test]
    fn the_working_set_fits_in_a_shard() {
        // Every URL the generator can name, at its size, in every variant
        // the nodes could keep and under every host spelling, with a
        // kilobyte of key and headers per object, must fit in one shard of
        // the nodes' budget, however the keys hash.
        const HOSTS: usize = 3;
        const OVERHEAD: usize = 1024;
        let mut bytes = 0;
        for (family, count, _) in FAMILIES {
            for i in 0..count as u64 {
                bytes += HOSTS
                    * match family {
                        "static" => {
                            let mut sizes = vec![origin::static_size(i, None)];
                            for q in QUERY_SIZES {
                                sizes.push(origin::static_size(i, Some(&format!("size={q}"))));
                            }
                            sizes.iter().map(|s| s + OVERHEAD).sum::<usize>()
                        }
                        // Over the object limit: never stored.
                        "big" => 0,
                        // Bodies under 100 bytes; five request variants.
                        "vary" | "lang" => 5 * (100 + OVERHEAD),
                        _ => 100 + OVERHEAD,
                    };
            }
        }
        let shard = super::super::node::MEMORY_BYTES / super::super::node::SHARDS;
        assert!(bytes < shard, "{bytes} bytes against a {shard}-byte shard");
    }

    #[test]
    fn parse_errors_name_the_line() {
        let ok = r#"{"t":1,"method":"GET","path":"/a"}"#;
        assert_eq!(parse(&format!("\n{ok}\n\n")).unwrap().len(), 1);
        assert_eq!(parse(ok).unwrap()[0].to, Target::Proxy);
        for (text, want) in [
            ("{", "line 1:"),
            (
                r#"{"t":1,"method":"GET","path":"/a","x":1}"#,
                "unknown field",
            ),
            (r#"{"t":1,"method":"G T","path":"/a"}"#, "invalid method"),
            (r#"{"t":1,"method":"GET","path":"a"}"#, "invalid path"),
            (r#"{"t":1,"method":"GET","path":"/a b"}"#, "invalid path"),
            (
                r#"{"t":1,"method":"GET","path":"/a","headers":[["a b","c"]]}"#,
                "invalid header name",
            ),
            (
                r#"{"t":1,"method":"GET","path":"/a","headers":[["a","c\n"]]}"#,
                "invalid value for header a",
            ),
            (
                r#"{"t":1,"to":"origin","method":"POST","path":"/x"}"#,
                "origin entries",
            ),
            (
                r#"{"t":1,"to":"elsewhere","method":"GET","path":"/a"}"#,
                "unknown variant",
            ),
        ] {
            let err = parse(text).unwrap_err();
            assert!(err.contains(want), "{text}: {err}");
        }
        let back = format!("{ok}\n{}", r#"{"t":0,"method":"GET","path":"/a"}"#);
        assert_eq!(
            parse(&back).unwrap_err(),
            "line 2: time goes back from 1 to 0"
        );
    }

    #[test]
    fn header_lookup_ignores_case() {
        let e = &generate(1, 1)[0];
        assert!(e.header("HOST").is_some() || e.to != Target::Proxy);
        assert_eq!(e.header("x-missing"), None);
    }
}
