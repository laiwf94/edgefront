//! The cache differential test: replays a recorded request log against
//! cachet 0.1 (the binary from before the workspace split) and the new
//! binary, side by side, and diffs every response.
//!
//! Both nodes run the same config against their own copy of a
//! deterministic origin ([`origin`]), on one logical clock ([`node`]).
//! The runner walks the log ([`log`]): it moves the clock to each entry's
//! time, applies origin events to both origins, and sends each request to
//! both nodes. A response is compared on
//!
//! - its status,
//! - every header but `Date`, by name and with the values of a repeated
//!   header in order (an `X-Request-Id` the node generated, because the
//!   request carried none, is random and compared only as "generated"),
//! - the SHA-256 of its body, and
//! - how many requests the node sent the origin to answer it.
//!
//! The last one catches a change in revalidation or collapsing that the
//! client cannot see in the response itself.
//!
//! The run is deterministic: requests go one at a time, the clock stands
//! still between log entries, and the generated working set never makes
//! a node evict. A divergence is a behaviour change, never noise.

pub mod cli;
pub mod log;
pub mod node;
pub mod origin;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant, UNIX_EPOCH};

use bytes::Bytes;
use edgefront_core::http1::{Client, ClientLimits};
use http::{HeaderName, HeaderValue, Method, Request};
use http_body_util::{BodyExt, Full};
use sha2::{Digest, Sha256};

use self::log::{Entry, HOST, Target};
use self::node::{Clock, Launch, Node};
use self::origin::Origin;

/// The logical time the log's `t = 0` maps to: 2026-11-09T00:00:00Z.
pub const START: u64 = 1_794_182_400;

/// The node name both binaries run with.
pub const NODE_NAME: &str = "difftest";

/// What one node answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub status: u16,
    /// Header names in order, each with its values in order.
    pub headers: BTreeMap<String, Vec<String>>,
    pub body_len: usize,
    pub body_sha256: String,
    /// Requests the node sent the origin while answering.
    pub origin_fetches: u64,
}

/// A response, or the client error that took its place.
pub type Outcome = Result<Observed, String>;

/// Strips what may differ between two correct runs: `Date`, and a request
/// id the node made up.
pub fn normalise(entry: &Entry, mut obs: Observed) -> Observed {
    obs.headers.remove("date");
    if let Some(ids) = obs.headers.get_mut("x-request-id") {
        let sent = entry.header("x-request-id");
        for id in ids.iter_mut() {
            let generated =
                id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) && sent != Some(id);
            if generated {
                *id = "<generated>".into();
            }
        }
    }
    obs
}

/// Every way `b` differs from `a`, one line each; empty when they agree.
pub fn diff(a: &Outcome, b: &Outcome) -> Vec<String> {
    let (a, b) = match (a, b) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(a), Err(b)) if a == b => return Vec::new(),
        (a, b) => return vec![format!("outcome: {} vs {}", summary(a), summary(b))],
    };
    let mut out = Vec::new();
    if a.status != b.status {
        out.push(format!("status: {} vs {}", a.status, b.status));
    }
    let names: std::collections::BTreeSet<&String> =
        a.headers.keys().chain(b.headers.keys()).collect();
    for name in names {
        let (x, y) = (a.headers.get(name), b.headers.get(name));
        if x != y {
            out.push(format!("{name}: {} vs {}", values(x), values(y)));
        }
    }
    if a.body_sha256 != b.body_sha256 {
        out.push(format!(
            "body: {} bytes sha256 {} vs {} bytes sha256 {}",
            a.body_len, a.body_sha256, b.body_len, b.body_sha256
        ));
    }
    if a.origin_fetches != b.origin_fetches {
        out.push(format!(
            "origin fetches: {} vs {}",
            a.origin_fetches, b.origin_fetches
        ));
    }
    out
}

fn summary(o: &Outcome) -> String {
    match o {
        Ok(obs) => format!("status {}", obs.status),
        Err(e) => format!("error {e}"),
    }
}

fn values(v: Option<&Vec<String>>) -> String {
    match v {
        None => "(absent)".into(),
        Some(v) => v
            .iter()
            .map(|s| format!("{s:?}"))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// The shape of a Cache-Status value without the cache name and the
/// numbers: `hit`, `fwd=miss; stored`, `fwd=stale; detail=stale-if-error`.
pub fn cache_status_kind(value: &str) -> String {
    value
        .split(';')
        .map(str::trim)
        .skip(1)
        .filter(|p| !p.starts_with("ttl=") && !p.starts_with("fwd-status="))
        .collect::<Vec<_>>()
        .join("; ")
}

/// One entry on which the nodes disagreed.
#[derive(Debug, Clone)]
pub struct Divergence {
    /// The entry's line in the log, from 1.
    pub line: usize,
    pub entry: Entry,
    pub differences: Vec<String>,
}

/// What a run found.
#[derive(Debug, Default)]
pub struct Report {
    /// Requests sent to each node.
    pub requests: usize,
    /// Origin events applied.
    pub events: usize,
    /// Entries on which the nodes disagreed.
    pub divergent: usize,
    /// The first divergences, up to the plan's limit.
    pub divergences: Vec<Divergence>,
    /// Statuses the reference answered, by code.
    pub statuses: BTreeMap<u16, usize>,
    /// Cache-Status kinds the reference answered (see [`cache_status_kind`]).
    pub cache_status: BTreeMap<String, usize>,
    /// Requests the reference sent its origin.
    pub origin_fetches: u64,
    /// Logical seconds the log covered.
    pub logical_secs: u64,
    pub elapsed: Duration,
}

impl Report {
    pub fn render(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "{} requests and {} origin events over {} logical seconds in {:.1}s: {} divergent",
            self.requests,
            self.events,
            self.logical_secs,
            self.elapsed.as_secs_f64(),
            self.divergent
        );
        let _ = writeln!(s, "origin fetches: {}", self.origin_fetches);
        let statuses: Vec<String> = self
            .statuses
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let _ = writeln!(s, "statuses: {}", statuses.join(" "));
        let _ = writeln!(s, "cache status:");
        for (kind, n) in &self.cache_status {
            let kind = if kind.is_empty() { "(none)" } else { kind };
            let _ = writeln!(s, "  {n:>7}  {kind}");
        }
        for d in &self.divergences {
            let _ = writeln!(
                s,
                "line {}: {}",
                d.line,
                serde_json::to_string(&d.entry).expect("entries serialise")
            );
            for diff in &d.differences {
                let _ = writeln!(s, "    {diff}");
            }
        }
        if self.divergent > self.divergences.len() {
            let _ = writeln!(
                s,
                "... and {} more",
                self.divergent - self.divergences.len()
            );
        }
        s
    }
}

/// A run to make.
#[derive(Debug, Clone)]
pub struct Plan<'a> {
    pub entries: &'a [Entry],
    /// cachet 0.1.
    pub reference: &'a Path,
    /// The binary under test.
    pub candidate: &'a Path,
    pub faketime: &'a Path,
    /// A scratch directory for configs, logs and the clock file.
    pub dir: &'a Path,
    /// Divergences to keep in the report.
    pub keep: usize,
}

struct Side {
    node: Node,
    origin: Origin,
}

fn http_date(secs: u64) -> String {
    httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(secs))
}

fn request(entry: &Entry, addr: SocketAddr) -> Result<Request<Full<Bytes>>, String> {
    let method = Method::from_bytes(entry.method.as_bytes()).map_err(|e| e.to_string())?;
    let mut req = Request::builder()
        .method(method)
        .uri(format!("http://{addr}{}", entry.path))
        .body(Full::new(Bytes::from(entry.body.clone())))
        .map_err(|e| e.to_string())?;
    let headers = req.headers_mut();
    for (name, value) in &entry.headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| e.to_string())?;
        let value = HeaderValue::from_str(value).map_err(|e| e.to_string())?;
        headers.append(name, value);
    }
    // The client would name the node's own address, which differs between
    // the two; a proxy request without a Host gets the log's host.
    if entry.to == Target::Proxy && !headers.contains_key(http::header::HOST) {
        headers.insert(http::header::HOST, HeaderValue::from_static(HOST));
    }
    Ok(req)
}

async fn observe(client: &Client<Full<Bytes>>, entry: &Entry, side: &Side) -> Outcome {
    let addr = match entry.to {
        Target::Admin => side.node.admin,
        _ => side.node.proxy,
    };
    let req = request(entry, addr)?;
    let before = side.origin.state.fetches();
    let resp = client.send(req).await.map_err(|e| e.as_str().to_string())?;
    let (parts, body) = resp.into_parts();
    let body = body
        .collect()
        .await
        .map_err(|_| "body".to_string())?
        .to_bytes();
    let mut headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in &parts.headers {
        headers
            .entry(name.as_str().to_string())
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    let digest = Sha256::digest(&body);
    Ok(normalise(
        entry,
        Observed {
            status: parts.status.as_u16(),
            headers,
            body_len: body.len(),
            body_sha256: digest.iter().map(|b| format!("{b:02x}")).collect(),
            origin_fetches: side.origin.state.fetches() - before,
        },
    ))
}

/// Checks that a node runs on the logical clock: its Date must be the
/// clock's time, not the wall clock's.
async fn check_clock(client: &Client<Full<Bytes>>, side: &Side, now: u64) -> io::Result<()> {
    let req = Request::builder()
        .uri(format!("http://{}/healthz", side.node.admin))
        .body(Full::new(Bytes::new()))
        .expect("valid request");
    let resp = client.send(req).await.map_err(|e| {
        side.node
            .failed(&format!("did not answer /healthz ({})", e.as_str()))
    })?;
    let date = resp
        .headers()
        .get(http::header::DATE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let _ = resp.into_body().collect().await;
    if date != http_date(now) {
        return Err(side.node.failed(&format!(
            "is not on the logical clock: Date was {date:?}, want {:?} (is libfaketime preloading?)",
            http_date(now)
        )));
    }
    Ok(())
}

/// Replays `plan.entries` against both binaries.
pub async fn run(plan: &Plan<'_>) -> io::Result<Report> {
    let started = Instant::now();
    std::fs::create_dir_all(plan.dir)?;
    let mut clock = Clock::create(plan.dir.join("now"), START)?;
    let mut sides = Vec::with_capacity(2);
    for (file, binary) in [("reference", plan.reference), ("candidate", plan.candidate)] {
        let origin = Origin::start(START).await?;
        let launch = Launch {
            binary,
            faketime: plan.faketime,
            dir: plan.dir,
            clock: &clock,
            origin: origin.addr,
        };
        let node = Node::start(NODE_NAME, file, &launch)?;
        sides.push(Side { node, origin });
    }
    let client = Client::new(&ClientLimits {
        response_header_timeout: Some(Duration::from_secs(30)),
        max_idle_per_host: 4,
        ..ClientLimits::default()
    });
    for side in &sides {
        check_clock(&client, side, START).await?;
    }
    let mut report = Report::default();
    for (i, entry) in plan.entries.iter().enumerate() {
        let now = START + entry.t;
        clock.set(now)?;
        for side in &sides {
            side.origin.state.set_now(now);
        }
        report.logical_secs = entry.t;
        if entry.to == Target::Origin {
            for side in &sides {
                side.origin
                    .state
                    .apply(&entry.path)
                    .map_err(io::Error::other)?;
            }
            report.events += 1;
            continue;
        }
        let (a, b) = tokio::join!(
            observe(&client, entry, &sides[0]),
            observe(&client, entry, &sides[1])
        );
        report.requests += 1;
        if let Ok(a) = &a {
            *report.statuses.entry(a.status).or_default() += 1;
            report.origin_fetches += a.origin_fetches;
            if let Some(cs) = a.headers.get("cache-status").and_then(|v| v.first()) {
                *report
                    .cache_status
                    .entry(cache_status_kind(cs))
                    .or_default() += 1;
            } else if entry.to == Target::Proxy {
                *report.cache_status.entry(String::new()).or_default() += 1;
            }
        }
        let differences = diff(&a, &b);
        if !differences.is_empty() {
            report.divergent += 1;
            if report.divergences.len() < plan.keep {
                report.divergences.push(Divergence {
                    line: i + 1,
                    entry: entry.clone(),
                    differences,
                });
            }
        }
    }
    for side in sides {
        drop(side.node);
        side.origin.stop().await;
    }
    report.elapsed = started.elapsed();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(status: u16, headers: &[(&str, &str)], body: &str) -> Observed {
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (n, v) in headers {
            map.entry(n.to_string()).or_default().push(v.to_string());
        }
        Observed {
            status,
            headers: map,
            body_len: body.len(),
            body_sha256: Sha256::digest(body.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            origin_fetches: 1,
        }
    }

    fn entry(headers: &[(&str, &str)]) -> Entry {
        Entry {
            t: 0,
            to: Target::Proxy,
            method: "GET".into(),
            path: "/a".into(),
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            body: String::new(),
        }
    }

    #[test]
    fn normalise_drops_date_and_generated_ids_only() {
        let generated = "0123456789abcdef0123456789abcdef";
        let o = obs(
            200,
            &[("date", "x"), ("x-request-id", generated), ("age", "3")],
            "",
        );
        let n = normalise(&entry(&[]), o.clone());
        assert_eq!(n.headers.get("date"), None);
        assert_eq!(n.headers["x-request-id"], ["<generated>"]);
        assert_eq!(n.headers["age"], ["3"]);
        // An id the client sent is compared as it is, even when it looks
        // generated.
        let n = normalise(&entry(&[("X-Request-Id", generated)]), o);
        assert_eq!(n.headers["x-request-id"], [generated]);
        let o = obs(200, &[("x-request-id", "r1")], "");
        assert_eq!(normalise(&entry(&[]), o).headers["x-request-id"], ["r1"]);
    }

    #[test]
    fn diff_names_each_difference() {
        let a: Outcome = Ok(obs(
            200,
            &[("age", "1"), ("via", "1.1 x"), ("via", "1.1 y")],
            "a",
        ));
        assert!(diff(&a, &a).is_empty());
        let mut b = obs(
            304,
            &[
                ("age", "2"),
                ("via", "1.1 y"),
                ("via", "1.1 x"),
                ("etag", "\"e\""),
            ],
            "b",
        );
        b.origin_fetches = 0;
        let d = diff(&a, &Ok(b));
        assert_eq!(d.len(), 6, "{d:?}");
        assert_eq!(d[0], "status: 200 vs 304");
        assert_eq!(d[1], "age: \"1\" vs \"2\"");
        assert_eq!(d[2], "etag: (absent) vs \"\\\"e\\\"\"");
        assert_eq!(d[3], "via: \"1.1 x\", \"1.1 y\" vs \"1.1 y\", \"1.1 x\"");
        assert!(
            d[4].starts_with("body: 1 bytes sha256 ca978112"),
            "{}",
            d[4]
        );
        assert_eq!(d[5], "origin fetches: 1 vs 0");
        let e: Outcome = Err("timeout".into());
        assert!(diff(&e, &e).is_empty());
        assert_eq!(diff(&a, &e), ["outcome: status 200 vs error timeout"]);
        assert_eq!(
            diff(&e, &Err("connect".into())),
            ["outcome: error timeout vs error connect"]
        );
    }

    #[test]
    fn cache_status_kinds() {
        assert_eq!(cache_status_kind("difftest; hit; ttl=12"), "hit");
        assert_eq!(
            cache_status_kind("difftest; fwd=miss; fwd-status=200; stored; ttl=300"),
            "fwd=miss; stored"
        );
        assert_eq!(
            cache_status_kind(
                "difftest; hit; ttl=-3; fwd=stale; fwd-status=503; detail=stale-if-error"
            ),
            "hit; fwd=stale; detail=stale-if-error"
        );
        assert_eq!(cache_status_kind("difftest"), "");
    }

    #[test]
    fn requests_carry_the_log_host_and_headers() {
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let mut e = entry(&[("accept", "a"), ("accept", "b")]);
        let r = request(&e, addr).unwrap();
        assert_eq!(r.uri(), "http://127.0.0.1:9/a");
        assert_eq!(r.headers()["host"], HOST);
        assert_eq!(r.headers().get_all("accept").iter().count(), 2);
        e.headers.push(("host".into(), "other.test".into()));
        assert_eq!(request(&e, addr).unwrap().headers()["host"], "other.test");
        e.to = Target::Admin;
        e.headers.clear();
        assert!(request(&e, addr).unwrap().headers().get("host").is_none());
        e.method = "G T".into();
        assert!(request(&e, addr).is_err());
    }

    #[test]
    fn reports_render() {
        let mut r = Report {
            requests: 3,
            events: 1,
            divergent: 2,
            logical_secs: 9,
            origin_fetches: 2,
            ..Report::default()
        };
        r.statuses.insert(200, 3);
        r.cache_status.insert("hit".into(), 2);
        r.cache_status.insert(String::new(), 1);
        r.divergences.push(Divergence {
            line: 4,
            entry: entry(&[]),
            differences: vec!["status: 200 vs 500".into()],
        });
        let text = r.render();
        assert!(
            text.starts_with("3 requests and 1 origin events over 9 logical seconds"),
            "{text}"
        );
        assert!(text.contains("2 divergent"));
        assert!(text.contains("statuses: 200=3"));
        assert!(text.contains("      2  hit\n"));
        assert!(text.contains("      1  (none)\n"));
        assert!(text.contains("line 4: {\"t\":0,\"to\":\"proxy\",\"method\":\"GET\",\"path\":\"/a\"}\n    status: 200 vs 500\n"));
        assert!(text.ends_with("... and 1 more\n"));
    }
}
