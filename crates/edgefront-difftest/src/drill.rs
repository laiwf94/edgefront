//! The tools of steer's failover drill, ported so the drill runs without
//! Go: the fake upstream of `legacy/steer/cmd/upstream` and the open-loop
//! load generator of `legacy/steer/cmd/loadgen`.
//!
//! The drill (`examples/failover.sh`, and `crates/edgefront/tests/
//! e2e_port_failover.rs` in process) puts two priority-0 upstreams and one
//! priority-1 backup behind the proxy, sends load at a constant rate, and
//! adds one fault per phase: a primary killed, the other primary failing
//! half its requests, then that primary blackholed. It prints the client's
//! success rate and latency per phase, in the table steer's README
//! publishes.
//!
//! [`Upstream`] serves an [`edgefront_proxy::fakeupstream::Server`] on a
//! traffic address and its [`Control`] on a control address, as
//! `cmd/upstream` did. [`Upstream::kill`] stops both at once and drops
//! every open connection, which is what the proxy sees when the process is
//! killed with SIGKILL; the shell drill kills the real process instead.
//!
//! [`load`] is `cmd/loadgen` with the same semantics, so the numbers are
//! comparable with steer's table:
//!
//! - Request `i` is due at `start + i × (1 s / rate)` whatever happened to
//!   earlier requests, and its latency runs from that time.
//! - Up to `max_inflight` requests are outstanding; one due while all are
//!   taken is counted as `dropped` and sent no more.
//! - Success is a 2xx response with its body read whole. A request that
//!   takes longer than `timeout` from the moment it is sent fails as
//!   `timeout`; one with no response fails as `transport_error`; any other
//!   status fails as `http_<status>`.
//! - The percentiles take the nearest rank over every request but the
//!   dropped ones, failures included at the time they failed, in
//!   milliseconds rounded to two places.
//! - [`Report::row`], [`Report::text`] and [`Report::json`] print what
//!   `-format row`, `text` and `json` printed.
//!
//! It speaks HTTP/1.1 over cleartext TCP only; steer's could also use
//! HTTPS, which the drill never did.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use edgefront_core::admin::go_json::{go_float, go_string};
use edgefront_core::config::go::parse_duration;
use edgefront_core::listener::{self, ServerLimits};
use edgefront_proxy::fakeupstream::{Control, Server};
use http::{HeaderName, HeaderValue, Method, Request, Uri};
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, sleep_until};

/// The most requests one load run may schedule, since every result is kept
/// for the percentiles.
pub const MAX_REQUESTS: f64 = 10_000_000.0;

/// `cmd/upstream`'s server limits: 5 seconds for a request head and 60
/// seconds idle.
fn upstream_limits() -> ServerLimits {
    ServerLimits {
        header_read_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(60),
        drain_timeout: Duration::ZERO,
        ..ServerLimits::default()
    }
}

/// A running fake upstream.
#[derive(Debug)]
pub struct Upstream {
    server: Arc<Server>,
    addr: SocketAddr,
    control: SocketAddr,
    tasks: Vec<JoinHandle<()>>,
}

impl Upstream {
    /// Listens on `addr` for traffic and on `control` for `/state`, and
    /// serves both until [`Upstream::kill`] or until dropped. Either may
    /// use port 0; [`Upstream::addr`] and [`Upstream::control`] say which
    /// port was given.
    pub async fn start(name: &str, addr: &str, control: &str) -> io::Result<Upstream> {
        let ln = TcpListener::bind(addr).await?;
        let cln = TcpListener::bind(control).await?;
        let server = Arc::new(Server::new(name));
        let (addr, control) = (ln.local_addr()?, cln.local_addr()?);
        let never = std::future::pending::<()>;
        let traffic = tokio::spawn({
            let server = server.clone();
            async move {
                listener::serve(ln, server, upstream_limits(), never()).await;
            }
        });
        let ctl = tokio::spawn({
            let control = Arc::new(Control(server.clone()));
            async move {
                listener::serve(cln, control, upstream_limits(), never()).await;
            }
        });
        Ok(Upstream {
            server,
            addr,
            control,
            tasks: vec![traffic, ctl],
        })
    }

    /// The traffic address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The control address.
    pub fn control(&self) -> SocketAddr {
        self.control
    }

    /// The server, whose faults [`Server::set`] changes as `PUT /state`
    /// would.
    pub fn server(&self) -> &Arc<Server> {
        &self.server
    }

    /// Stops listening and drops every open connection, requests in
    /// flight and held by the blackhole included, without an answer.
    /// Returns once both listeners are closed.
    pub async fn kill(mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// How to run [`load`]: `cmd/loadgen`'s flags.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadOptions {
    /// The target, an `http://` URL.
    pub url: Uri,
    /// Requests per second.
    pub rate: f64,
    /// How long to send.
    pub duration: Duration,
    /// How long one request may take; longer fails as `timeout`.
    pub timeout: Duration,
    /// The request method.
    pub method: Method,
    /// Extra request headers.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// Requests outstanding before new ones are dropped.
    pub max_inflight: usize,
    /// The first column of [`Report::row`].
    pub label: String,
}

impl LoadOptions {
    /// `cmd/loadgen`'s defaults for `url`: 200 requests per second for 10
    /// seconds, a 2-second timeout, GET and 10,000 in flight.
    pub fn new(url: Uri) -> LoadOptions {
        LoadOptions {
            url,
            rate: 200.0,
            duration: Duration::from_secs(10),
            timeout: Duration::from_secs(2),
            method: Method::GET,
            headers: Vec::new(),
            max_inflight: 10_000,
            label: String::new(),
        }
    }

    /// Checks the options as `cmd/loadgen` did, with its messages.
    pub fn validate(&self) -> Result<(), String> {
        if self.url.scheme_str() != Some("http") || self.url.authority().is_none() {
            return Err("-url must be an http:// URL".into());
        }
        let secs = self.duration.as_secs_f64();
        if !(self.rate > 0.0 && self.rate <= 1_000_000.0) {
            return Err("-rate must be between 0 and 1000000".into());
        }
        if self.duration.is_zero() || self.duration > Duration::from_secs(24 * 3600) {
            return Err("-duration must be between 0 and 24h".into());
        }
        if self.timeout.is_zero() {
            return Err("-timeout must be positive".into());
        }
        if self.max_inflight < 1 {
            return Err("-max-inflight must be at least 1".into());
        }
        if self.rate * secs > MAX_REQUESTS {
            return Err(format!(
                "-rate x -duration must not exceed {MAX_REQUESTS} requests"
            ));
        }
        Ok(())
    }

    /// The number of requests a run schedules, `int(rate × seconds)`.
    pub fn requests(&self) -> u64 {
        (self.rate * self.duration.as_secs_f64()) as u64
    }

    /// The time between two due times, truncated to the nanosecond as Go's
    /// `time.Duration(float64(time.Second) / rate)` is.
    pub fn interval(&self) -> Duration {
        Duration::from_nanos((1e9 / self.rate) as u64)
    }
}

/// How one request ended.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sample {
    took: Duration,
    outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Ok,
    Failed(String),
    Dropped,
}

/// A run's summary, `cmd/loadgen`'s `Report`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Report {
    /// [`LoadOptions::label`].
    pub label: String,
    /// Requests scheduled, dropped ones included.
    pub requests: u64,
    /// Requests that got a 2xx.
    pub ok: u64,
    /// `ok` as a percentage of `requests`, rounded to two places.
    pub success_percent: f64,
    /// Failed and dropped requests by kind.
    pub failures: BTreeMap<String, u64>,
    /// The median latency in milliseconds.
    pub p50_ms: f64,
    /// The 99th percentile.
    pub p99_ms: f64,
    /// The 99.9th percentile.
    pub p999_ms: f64,
    /// The slowest request.
    pub max_ms: f64,
    /// How long the run took, in seconds rounded to two places.
    pub elapsed_s: f64,
}

/// Milliseconds rounded to two places, from whole microseconds.
fn ms(d: Duration) -> f64 {
    (d.as_micros() as f64 / 10.0).round() / 100.0
}

/// The nearest-rank percentile of `sorted`.
fn percentile(sorted: &[Duration], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    ms(sorted[rank.saturating_sub(1)])
}

impl Report {
    fn summarise(samples: &[Sample], elapsed: Duration) -> Report {
        let mut r = Report {
            requests: samples.len() as u64,
            elapsed_s: (elapsed.as_secs_f64() * 100.0).round() / 100.0,
            ..Report::default()
        };
        let mut ds = Vec::with_capacity(samples.len());
        for s in samples {
            match &s.outcome {
                Outcome::Ok => r.ok += 1,
                Outcome::Failed(kind) => *r.failures.entry(kind.clone()).or_default() += 1,
                Outcome::Dropped => *r.failures.entry("dropped".into()).or_default() += 1,
            }
            if s.outcome != Outcome::Dropped {
                ds.push(s.took);
            }
        }
        if r.requests > 0 {
            r.success_percent = (10_000.0 * r.ok as f64 / r.requests as f64).round() / 100.0;
        }
        ds.sort_unstable();
        r.p50_ms = percentile(&ds, 0.5);
        r.p99_ms = percentile(&ds, 0.99);
        r.p999_ms = percentile(&ds, 0.999);
        r.max_ms = ds.last().map_or(0.0, |d| ms(*d));
        r
    }

    /// One row of the drill's Markdown table (`-format row`).
    pub fn row(&self) -> String {
        format!(
            "| {} | {} | {:.2}% | {:.1} ms | {:.1} ms | {:.1} ms |\n",
            self.label, self.requests, self.success_percent, self.p50_ms, self.p99_ms, self.p999_ms
        )
    }

    /// Two lines for a person (`-format text`).
    pub fn text(&self) -> String {
        let failures = if self.failures.is_empty() {
            "none".to_owned()
        } else {
            let kinds: Vec<String> = self
                .failures
                .iter()
                .map(|(k, v)| format!("{k} x{v}"))
                .collect();
            kinds.join(", ")
        };
        format!(
            "requests {} in {:.1}s  success {:.2}%  ({} ok; failures: {failures})\n\
             latency  p50 {:.1} ms  p99 {:.1} ms  p999 {:.1} ms  max {:.1} ms\n",
            self.requests,
            self.elapsed_s,
            self.success_percent,
            self.ok,
            self.p50_ms,
            self.p99_ms,
            self.p999_ms,
            self.max_ms
        )
    }

    /// One JSON document as Go's encoder wrote the `Report` struct
    /// (`-format json`).
    pub fn json(&self) -> String {
        let mut out = String::from("{");
        if !self.label.is_empty() {
            let _ = write!(out, "\"label\":{},", go_string(&self.label));
        }
        let failures: Vec<String> = self
            .failures
            .iter()
            .map(|(k, v)| format!("{}:{v}", go_string(k)))
            .collect();
        let _ = writeln!(
            out,
            "\"requests\":{},\"ok\":{},\"success_percent\":{},\"failures\":{{{}}},\
             \"p50_ms\":{},\"p99_ms\":{},\"p999_ms\":{},\"max_ms\":{},\"elapsed_s\":{}}}",
            self.requests,
            self.ok,
            go_float(self.success_percent),
            failures.join(","),
            go_float(self.p50_ms),
            go_float(self.p99_ms),
            go_float(self.p999_ms),
            go_float(self.max_ms),
            go_float(self.elapsed_s)
        );
        out
    }

    /// The report in `format`: `text`, `row` or `json`.
    pub fn format(&self, format: &str) -> Option<String> {
        match format {
            "text" => Some(self.text()),
            "row" => Some(self.row()),
            "json" => Some(self.json()),
            _ => None,
        }
    }
}

type HttpClient = Client<HttpConnector, Empty<Bytes>>;

/// Sends one request and reads its body.
async fn send(client: HttpClient, o: Arc<LoadOptions>, due: Instant) -> Sample {
    let mut req = Request::new(Empty::new());
    *req.method_mut() = o.method.clone();
    *req.uri_mut() = o.url.clone();
    for (name, value) in &o.headers {
        req.headers_mut().append(name.clone(), value.clone());
    }
    let exchange = async {
        let resp = client.request(req).await.map_err(|_| ())?;
        let status = resp.status();
        resp.into_body().collect().await.map_err(|_| ())?;
        Ok::<_, ()>(status)
    };
    let outcome = match tokio::time::timeout(o.timeout, exchange).await {
        Err(_) => Outcome::Failed("timeout".into()),
        Ok(Err(())) => Outcome::Failed("transport_error".into()),
        Ok(Ok(status)) if status.is_success() => Outcome::Ok,
        Ok(Ok(status)) => Outcome::Failed(format!("http_{}", status.as_u16())),
    };
    Sample {
        took: due.elapsed(),
        outcome,
    }
}

/// Sends `o.requests()` requests open loop and summarises them. Call
/// [`LoadOptions::validate`] first; an invalid rate schedules nothing.
pub async fn load(o: &LoadOptions) -> Report {
    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    let client: HttpClient = Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(o.max_inflight)
        .pool_idle_timeout(Duration::from_secs(30))
        .build(connector);
    let o = Arc::new(o.clone());
    let slots = Arc::new(Semaphore::new(o.max_inflight));
    let interval = u64::try_from(o.interval().as_nanos()).unwrap_or(u64::MAX);
    let total = if o.rate > 0.0 { o.requests() } else { 0 };
    let mut samples = Vec::with_capacity(usize::try_from(total).unwrap_or(0));
    let mut sending = JoinSet::new();
    let start = Instant::now();
    for i in 0..total {
        let due = start + Duration::from_nanos(interval.saturating_mul(i));
        sleep_until(due).await;
        while let Some(done) = sending.try_join_next() {
            samples.extend(done.ok());
        }
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            samples.push(Sample {
                took: Duration::ZERO,
                outcome: Outcome::Dropped,
            });
            continue;
        };
        let (client, o) = (client.clone(), o.clone());
        sending.spawn(async move {
            let s = send(client, o, due).await;
            drop(slot);
            s
        });
    }
    while let Some(done) = sending.join_next().await {
        samples.extend(done.ok());
    }
    let mut r = Report::summarise(&samples, start.elapsed());
    r.label = o.label.clone();
    r
}

/// Splits Go-style flags (`-name value`, `--name value`, `-name=value`)
/// for the flags in `known`, each of which takes a value. Repeated flags
/// keep every value, in order.
fn go_flags(args: &[String], known: &[&str]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let Some(flag) = arg
            .strip_prefix("--")
            .or_else(|| arg.strip_prefix('-'))
            .filter(|f| !f.is_empty() && !f.starts_with('-'))
        else {
            return Err(format!("unexpected arguments: {arg}"));
        };
        let (name, value) = match flag.split_once('=') {
            Some((n, v)) => (n, v.to_owned()),
            None => {
                let value = it
                    .next()
                    .ok_or_else(|| format!("flag needs an argument: -{flag}"))?;
                (flag, value.clone())
            }
        };
        if !known.contains(&name) {
            return Err(format!("flag provided but not defined: -{name}"));
        }
        out.push((name.to_owned(), value));
    }
    Ok(out)
}

/// A Go duration flag.
fn duration_flag(name: &str, v: &str) -> Result<Duration, String> {
    match parse_duration(v) {
        Some(n) if n >= 0 => Ok(Duration::from_nanos(n as u64)),
        Some(_) => Ok(Duration::ZERO),
        None => Err(format!("invalid value {v:?} for flag -{name}: parse error")),
    }
}

/// Splits `"Name: value"` as `cmd/loadgen`'s `-H` did.
fn parse_header(s: &str) -> Result<(HeaderName, HeaderValue), String> {
    let bad = || format!("header {s:?} must look like \"Name: value\"");
    let (name, value) = s.split_once(':').ok_or_else(bad)?;
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() || s.len() > 8 << 10 {
        return Err(bad());
    }
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| format!("header name {name:?} is not a valid token"))?;
    if value.bytes().any(|c| (c < b' ' && c != b'\t') || c == 0x7f) {
        return Err(format!(
            "header value for {:?} contains control characters",
            name.as_str()
        ));
    }
    let value = HeaderValue::from_str(value)
        .map_err(|_| format!("header value for {:?} is not valid", name.as_str()))?;
    Ok((name, value))
}

/// Parses `edgefront-difftest loadgen`'s flags, `cmd/loadgen`'s: `-url`
/// (required), `-rate`, `-duration`, `-timeout`, `-method`, `-H`
/// (repeatable), `-max-inflight`, `-format` and `-label`. Returns the
/// options and the format.
pub fn parse_load_args(args: &[String]) -> Result<(LoadOptions, String), String> {
    let flags = go_flags(
        args,
        &[
            "url",
            "rate",
            "duration",
            "timeout",
            "method",
            "H",
            "max-inflight",
            "format",
            "label",
        ],
    )?;
    let mut url = None;
    let mut o = LoadOptions::new(Uri::from_static("http://placeholder/"));
    let mut format = "text".to_owned();
    for (name, v) in flags {
        match name.as_str() {
            "url" => url = Some(v),
            "rate" => {
                o.rate = v
                    .parse()
                    .map_err(|_| format!("invalid value {v:?} for flag -rate: parse error"))?;
            }
            "duration" => o.duration = duration_flag(&name, &v)?,
            "timeout" => o.timeout = duration_flag(&name, &v)?,
            "method" => {
                if v.is_empty() || !v.bytes().all(|c| c.is_ascii_uppercase()) {
                    return Err("-method must be an upper-case HTTP method".into());
                }
                o.method = Method::from_bytes(v.as_bytes())
                    .map_err(|_| "-method must be an upper-case HTTP method".to_owned())?;
            }
            "H" => o.headers.push(parse_header(&v)?),
            "max-inflight" => {
                let n: i64 = v.parse().map_err(|_| {
                    format!("invalid value {v:?} for flag -max-inflight: parse error")
                })?;
                o.max_inflight = usize::try_from(n).unwrap_or(0);
            }
            "format" => format = v,
            _ => o.label = v,
        }
    }
    let url = url.unwrap_or_default();
    o.url = url
        .parse()
        .map_err(|_| "-url must be an http:// URL".to_owned())?;
    o.validate()?;
    if !matches!(format.as_str(), "text" | "row" | "json") {
        return Err("-format must be text, row or json".into());
    }
    Ok((o, format))
}

/// `edgefront-difftest upstream`'s flags, `cmd/upstream`'s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamArgs {
    /// `-name`: the name in responses, by default the traffic address.
    pub name: String,
    /// `-addr`: the traffic address (default 127.0.0.1:9001).
    pub addr: String,
    /// `-control`: the control address (default 127.0.0.1:19001).
    pub control: String,
}

/// Parses `-addr`, `-control` and `-name`.
pub fn parse_upstream_args(args: &[String]) -> Result<UpstreamArgs, String> {
    let mut a = UpstreamArgs {
        name: String::new(),
        addr: "127.0.0.1:9001".into(),
        control: "127.0.0.1:19001".into(),
    };
    for (name, v) in go_flags(args, &["addr", "control", "name"])? {
        match name.as_str() {
            "addr" => a.addr = v,
            "control" => a.control = v,
            _ => a.name = v,
        }
    }
    if a.name.is_empty() {
        a.name = a.addr.clone();
    }
    Ok(a)
}

/// The line `cmd/upstream` logged once it listened.
pub fn listening_line(name: &str, up: &Upstream) -> String {
    format!(
        "{{\"level\":\"INFO\",\"msg\":\"listening\",\"name\":{},\"addr\":\"{}\",\"control\":\"{}\"}}\n",
        go_string(name),
        up.addr(),
        up.control()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    fn sample(ms: u64, outcome: Outcome) -> Sample {
        Sample {
            took: Duration::from_millis(ms),
            outcome,
        }
    }

    #[test]
    fn flags_are_split_as_go_splits_them() {
        let f = go_flags(
            &args("-rate 5 --url=http://x/ -H a:b -H c:d"),
            &["rate", "url", "H"],
        )
        .unwrap();
        assert_eq!(
            f,
            [
                ("rate".into(), "5".into()),
                ("url".into(), "http://x/".into()),
                ("H".into(), "a:b".into()),
                ("H".into(), "c:d".into())
            ]
        );
        assert_eq!(
            go_flags(&args("-nope 1"), &["rate"]).unwrap_err(),
            "flag provided but not defined: -nope"
        );
        assert_eq!(
            go_flags(&args("-rate"), &["rate"]).unwrap_err(),
            "flag needs an argument: -rate"
        );
        assert_eq!(
            go_flags(&args("stray"), &["rate"]).unwrap_err(),
            "unexpected arguments: stray"
        );
        assert!(go_flags(&args("---rate 1"), &["rate"]).is_err());
    }

    #[test]
    fn load_flags_take_loadgens_defaults_and_bounds() {
        let (o, format) = parse_load_args(&args("-url http://127.0.0.1:1/")).unwrap();
        assert_eq!(format, "text");
        assert_eq!(o.rate, 200.0);
        assert_eq!(o.duration, Duration::from_secs(10));
        assert_eq!(o.timeout, Duration::from_secs(2));
        assert_eq!(o.max_inflight, 10_000);
        assert_eq!(o.requests(), 2_000);
        assert_eq!(o.interval(), Duration::from_millis(5));

        let (o, format) = parse_load_args(&args(
            "-url http://h/p -rate 3 -duration 1.5s -timeout 250ms -method POST \
             -H X-A:1 -max-inflight 7 -format row -label phase",
        ))
        .unwrap();
        assert_eq!(
            (
                o.rate,
                o.duration,
                o.timeout,
                o.max_inflight,
                format.as_str()
            ),
            (
                3.0,
                Duration::from_millis(1500),
                Duration::from_millis(250),
                7,
                "row"
            )
        );
        assert_eq!((&o.method, o.label.as_str()), (&Method::POST, "phase"));
        assert_eq!(o.headers[0].0, "x-a");
        assert_eq!(o.requests(), 4);
        assert_eq!(o.interval(), Duration::from_nanos(333_333_333));

        for (bad, message) in [
            ("", "-url must be an http:// URL"),
            ("-url https://h/", "-url must be an http:// URL"),
            ("-url /path", "-url must be an http:// URL"),
            (
                "-url http://h/ -rate 0",
                "-rate must be between 0 and 1000000",
            ),
            (
                "-url http://h/ -rate NaN",
                "-rate must be between 0 and 1000000",
            ),
            (
                "-url http://h/ -rate 2000000",
                "-rate must be between 0 and 1000000",
            ),
            (
                "-url http://h/ -duration 0s",
                "-duration must be between 0 and 24h",
            ),
            (
                "-url http://h/ -duration -1s",
                "-duration must be between 0 and 24h",
            ),
            (
                "-url http://h/ -duration 25h",
                "-duration must be between 0 and 24h",
            ),
            ("-url http://h/ -timeout 0s", "-timeout must be positive"),
            (
                "-url http://h/ -max-inflight 0",
                "-max-inflight must be at least 1",
            ),
            (
                "-url http://h/ -rate 1000000 -duration 11s",
                "-rate x -duration must not exceed 10000000 requests",
            ),
            (
                "-url http://h/ -format xml",
                "-format must be text, row or json",
            ),
            (
                "-url http://h/ -method get",
                "-method must be an upper-case HTTP method",
            ),
            ("-url http://h/ -method", "flag needs an argument: -method"),
            (
                "-url http://h/ -rate x",
                "invalid value \"x\" for flag -rate: parse error",
            ),
            (
                "-url http://h/ -duration 5",
                "invalid value \"5\" for flag -duration: parse error",
            ),
            (
                "-url http://h/ -max-inflight -1",
                "-max-inflight must be at least 1",
            ),
            (
                "-url http://h/ -max-inflight x",
                "invalid value \"x\" for flag -max-inflight: parse error",
            ),
            (
                "-url http://h/ -H nocolon",
                "header \"nocolon\" must look like \"Name: value\"",
            ),
            (
                "-url http://h/ -H :v",
                "header \":v\" must look like \"Name: value\"",
            ),
            (
                "-url http://h/ -H a(b:v",
                "header name \"a(b\" is not a valid token",
            ),
        ] {
            assert_eq!(parse_load_args(&args(bad)).unwrap_err(), message, "{bad}");
        }
        let control = vec![
            "-url".into(),
            "http://h/".into(),
            "-H".into(),
            "a: b\u{1}".into(),
        ];
        assert_eq!(
            parse_load_args(&control).unwrap_err(),
            "header value for \"a\" contains control characters"
        );
        let long = vec![
            "-url".into(),
            "http://h/".into(),
            "-H".into(),
            format!("a: {}", "v".repeat(9000)),
        ];
        assert!(
            parse_load_args(&long)
                .unwrap_err()
                .contains("must look like")
        );
        let utf8 = vec![
            "-url".into(),
            "http://h/".into(),
            "-H".into(),
            "a: é".into(),
        ];
        assert_eq!(
            parse_load_args(&utf8).unwrap().0.headers[0].1.as_bytes(),
            "é".as_bytes()
        );
    }

    #[test]
    fn upstream_flags_default_like_cmd_upstream() {
        let a = parse_upstream_args(&[]).unwrap();
        assert_eq!(
            (a.name.as_str(), a.addr.as_str(), a.control.as_str()),
            ("127.0.0.1:9001", "127.0.0.1:9001", "127.0.0.1:19001")
        );
        let a =
            parse_upstream_args(&args("-name b -addr 127.0.0.1:1 -control=127.0.0.1:2")).unwrap();
        assert_eq!(
            (a.name.as_str(), a.addr.as_str(), a.control.as_str()),
            ("b", "127.0.0.1:1", "127.0.0.1:2")
        );
        assert!(parse_upstream_args(&args("-rate 1")).is_err());
    }

    #[test]
    fn reports_round_and_rank_as_loadgen_did() {
        let mut samples: Vec<Sample> = (1..=1000).map(|i| sample(i, Outcome::Ok)).collect();
        samples[0].outcome = Outcome::Failed("http_503".into());
        samples[1].outcome = Outcome::Failed("timeout".into());
        samples.push(sample(0, Outcome::Dropped));
        let r = Report::summarise(&samples, Duration::from_millis(10_004));
        assert_eq!((r.requests, r.ok), (1001, 998));
        assert_eq!(r.success_percent, 99.7);
        assert_eq!(
            r.failures,
            BTreeMap::from([
                ("dropped".into(), 1),
                ("http_503".into(), 1),
                ("timeout".into(), 1)
            ])
        );
        // Ranks over the 1,000 that were sent, failures included.
        assert_eq!(
            (r.p50_ms, r.p99_ms, r.p999_ms, r.max_ms),
            (500.0, 990.0, 999.0, 1000.0)
        );
        assert_eq!(r.elapsed_s, 10.0);
        assert_eq!(
            r.row(),
            "|  | 1001 | 99.70% | 500.0 ms | 990.0 ms | 999.0 ms |\n"
        );
        assert_eq!(
            r.text(),
            "requests 1001 in 10.0s  success 99.70%  (998 ok; failures: dropped x1, http_503 x1, timeout x1)\n\
             latency  p50 500.0 ms  p99 990.0 ms  p999 999.0 ms  max 1000.0 ms\n"
        );
        assert_eq!(
            r.json(),
            "{\"requests\":1001,\"ok\":998,\"success_percent\":99.7,\
             \"failures\":{\"dropped\":1,\"http_503\":1,\"timeout\":1},\
             \"p50_ms\":500,\"p99_ms\":990,\"p999_ms\":999,\"max_ms\":1000,\"elapsed_s\":10}\n"
        );
        assert_eq!(r.format("row"), Some(r.row()));
        assert_eq!(r.format("json"), Some(r.json()));
        assert_eq!(r.format("text"), Some(r.text()));
        assert_eq!(r.format("csv"), None);

        // Microseconds round to two places of a millisecond.
        assert_eq!(ms(Duration::from_micros(1_234_567)), 1234.57);
        assert_eq!(ms(Duration::from_nanos(4_999)), 0.0);
        assert_eq!(ms(Duration::from_nanos(5_000)), 0.01);

        let empty = Report::summarise(&[], Duration::ZERO);
        assert_eq!(
            (empty.success_percent, empty.p99_ms, empty.max_ms),
            (0.0, 0.0, 0.0)
        );
        assert_eq!(
            empty.text().lines().next().unwrap(),
            "requests 0 in 0.0s  success 0.00%  (0 ok; failures: none)"
        );
        let labelled = Report {
            label: "1. <base>".into(),
            ..empty
        };
        assert!(
            labelled
                .json()
                .starts_with("{\"label\":\"1. \\u003cbase\\u003e\",\"requests\":0,")
        );
    }
}
