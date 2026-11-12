//! An HTTP upstream whose failures can be dialled in at run time, ported
//! from steer's `internal/fakeupstream`: added latency, a fraction of 503
//! responses, and a blackhole mode that accepts requests and never answers.
//! The proxy's tests use it for failure injection, and the failover drill
//! serves it as the upstreams it breaks.
//!
//! `/healthz` answers 200 subject to latency and blackhole but not to the
//! error rate, like a real service whose health endpoint stays green while
//! a dependency fails half its requests. That is the case only passive
//! outlier detection can catch.
//!
//! [`Server`] answers traffic and [`Control`] answers `GET /state` and
//! `PUT /state` with a JSON patch of [`Settings`]. Both implement
//! [`Handler`], so they serve on [`listener::serve`]; both also answer a
//! request with any body through `handle`, which tests call directly.
//!
//! The bodies are byte for byte what steer wrote: Go's `encoding/json`
//! sorts map keys, escapes `<`, `>` and `&`, and ends each document with a
//! newline, and so does this module.
//!
//! [`listener::serve`]: edgefront_core::listener::serve

use std::fmt;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use edgefront_core::config::go;
use edgefront_core::listener::Handler;
use http::header::{ALLOW, CONTENT_TYPE, HeaderValue};
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Incoming};
use serde::Deserializer as _;
use serde::de::{self, MapAccess, Visitor};
use tokio::sync::watch;

use crate::balancer::rng::SplitMix64;
use crate::path;

/// The largest control request body accepted.
pub const MAX_CONTROL_BODY: usize = 4 << 10;
/// The largest latency a control request may set.
pub const MAX_LATENCY: Duration = Duration::from_secs(60);
/// How much of a traffic request's body is read before answering.
const MAX_REQUEST_BODY: usize = 10 << 20;

/// The injected faults.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Settings {
    /// Added before every answer, `/healthz` included.
    pub latency: Duration,
    /// The fraction of requests, 0 to 1, answered with 503. `/healthz` is
    /// exempt.
    pub error_rate: f64,
    /// Accept requests and never answer them.
    pub blackhole: bool,
}

impl Settings {
    /// The settings as steer rendered them: latency as a Go duration
    /// string, keys sorted.
    pub fn to_json(&self) -> String {
        let latency = i64::try_from(self.latency.as_nanos()).unwrap_or(i64::MAX);
        format!(
            "{{\"blackhole\":{},\"error_rate\":{},\"latency\":{}}}",
            self.blackhole,
            go_float(self.error_rate),
            go_string(&go::format_duration(latency)),
        )
    }
}

/// Applies a JSON patch such as `{"error_rate":0.5}` on top of `cur`, as
/// steer's `ParseSettings` did. Unknown fields, trailing data, values of
/// the wrong type and out-of-range values are rejected; on error `cur` is
/// unchanged.
///
/// It follows Go's `encoding/json` into a struct of optional fields: field
/// names match without regard to case, a later duplicate overrides an
/// earlier one, `null` leaves a field as it was, and a top-level `null`
/// is an empty patch.
pub fn parse_settings(cur: Settings, data: &[u8]) -> Result<Settings, String> {
    if data.len() > MAX_CONTROL_BODY {
        return Err(format!("body is larger than {MAX_CONTROL_BODY} bytes"));
    }
    let patch = parse_patch(data)?;
    let mut next = cur;
    if !patch.latency.is_empty() {
        let max = i64::try_from(MAX_LATENCY.as_nanos()).unwrap_or(i64::MAX);
        let range = || {
            format!(
                "latency must be a duration between 0s and {}",
                go::format_duration(max)
            )
        };
        let nanos = go::parse_duration(&patch.latency).ok_or_else(range)?;
        if !(0..=max).contains(&nanos) {
            return Err(range());
        }
        next.latency = Duration::from_nanos(nanos.unsigned_abs());
    }
    if let Some(rate) = patch.error_rate {
        if !(0.0..=1.0).contains(&rate) {
            return Err("error_rate must be between 0 and 1".to_owned());
        }
        next.error_rate = rate;
    }
    if let Some(blackhole) = patch.blackhole {
        next.blackhole = blackhole;
    }
    Ok(next)
}

/// steer's `wireSettings`: a string that `null` leaves alone, and two
/// pointers that `null` clears.
#[derive(Debug, Default)]
struct Patch {
    latency: String,
    error_rate: Option<f64>,
    blackhole: Option<bool>,
}

fn parse_patch(data: &[u8]) -> Result<Patch, String> {
    let mut de = serde_json::Deserializer::from_slice(data);
    let patch = de
        .deserialize_any(PatchVisitor)
        .and_then(|p| de.end().map(|()| p));
    patch.map_err(|e| {
        if e.classify() == serde_json::error::Category::Syntax
            && e.to_string().starts_with("trailing")
        {
            "body must be a single JSON object".to_owned()
        } else {
            format!("invalid JSON: {e}")
        }
    })
}

struct PatchVisitor;

impl<'de> Visitor<'de> for PatchVisitor {
    type Value = Patch;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Patch, E> {
        Ok(Patch::default())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Patch, A::Error> {
        let mut p = Patch::default();
        while let Some(key) = map.next_key::<String>()? {
            match fold(&key).as_str() {
                "LATENCY" => {
                    if let Some(s) = map.next_value::<Option<String>>()? {
                        p.latency = s;
                    }
                }
                "ERROR_RATE" => p.error_rate = map.next_value()?,
                "BLACKHOLE" => p.blackhole = map.next_value()?,
                _ => {
                    return Err(de::Error::custom(format!(
                        "unknown field {}",
                        go_string(&key)
                    )));
                }
            }
        }
        Ok(p)
    }
}

/// Go's field-name folding: each character lowered and then raised, so
/// that `Latency`, `LATENCY` and `latency` are one name, and so are the
/// Kelvin sign and `k`, or the long s and `s`.
fn fold(name: &str) -> String {
    name.chars()
        .map(|c| {
            let lower = single(c.to_lowercase()).unwrap_or(c);
            single(lower.to_uppercase()).unwrap_or(lower)
        })
        .collect()
}

fn single(mut it: impl Iterator<Item = char>) -> Option<char> {
    let c = it.next()?;
    it.next().is_none().then_some(c)
}

/// One fake upstream.
pub struct Server {
    name: String,
    settings: Mutex<Settings>,
    closed: watch::Sender<bool>,
    rng: SplitMix64,
    served: AtomicI64,
    failed: AtomicI64,
    held: AtomicI64,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (served, failed, held) = self.counts();
        f.debug_struct("Server")
            .field("name", &self.name)
            .field("settings", &self.settings())
            .field("served", &served)
            .field("failed", &failed)
            .field("held", &held)
            .finish()
    }
}

impl Server {
    /// A healthy upstream that identifies itself as `name`.
    pub fn new(name: &str) -> Server {
        Server::with_rng(name, SplitMix64::from_entropy())
    }

    /// A healthy upstream whose injected failures follow `seed`.
    pub fn with_seed(name: &str, seed: u64) -> Server {
        Server::with_rng(name, SplitMix64::new(seed))
    }

    fn with_rng(name: &str, rng: SplitMix64) -> Server {
        Server {
            name: name.to_owned(),
            settings: Mutex::new(Settings::default()),
            closed: watch::Sender::new(false),
            rng,
            served: AtomicI64::new(0),
            failed: AtomicI64::new(0),
            held: AtomicI64::new(0),
        }
    }

    /// The name it answers with.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The current faults.
    pub fn settings(&self) -> Settings {
        *self.settings.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replaces the faults.
    pub fn set(&self, s: Settings) {
        *self.settings.lock().unwrap_or_else(PoisonError::into_inner) = s;
    }

    /// Releases requests held by blackhole mode, now and from now on.
    pub fn close(&self) {
        self.closed.send_replace(true);
    }

    /// Requests answered, requests failed on purpose, and requests held by
    /// the blackhole.
    pub fn counts(&self) -> (i64, i64, i64) {
        (
            self.served.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
            self.held.load(Ordering::Relaxed),
        )
    }

    /// Answers one traffic request. Dropping the future is the client
    /// giving up: a held or delayed request then ends without an answer.
    pub async fn handle<B: Body>(&self, req: Request<B>) -> Response<Full<Bytes>> {
        let (parts, body) = req.into_parts();
        let _ = read_limited(body, MAX_REQUEST_BODY).await;
        // Go's server answers a path it cannot decode before any handler.
        let Ok(path) = path::unescape(parts.uri.path().as_bytes()) else {
            return text(StatusCode::BAD_REQUEST, "400 Bad Request");
        };
        let path = String::from_utf8_lossy(&path).into_owned();
        let cfg = self.settings();
        if cfg.blackhole {
            self.held.fetch_add(1, Ordering::Relaxed);
            // Never answer; wait for the client to give up, or for close.
            let _ = self.closed.subscribe().wait_for(|closed| *closed).await;
            return Response::new(Full::default());
        }
        if !cfg.latency.is_zero() {
            tokio::time::sleep(cfg.latency).await;
        }
        let (status, body) = if path == "/healthz" {
            (StatusCode::OK, "{\"status\":\"ok\"}\n".to_owned())
        } else if cfg.error_rate > 0.0 && self.random() < cfg.error_rate {
            self.failed.fetch_add(1, Ordering::Relaxed);
            let body = format!(
                "{{\"error\":\"injected failure\",\"upstream\":{}}}\n",
                go_string(&self.name)
            );
            (StatusCode::SERVICE_UNAVAILABLE, body)
        } else {
            self.served.fetch_add(1, Ordering::Relaxed);
            let body = format!(
                "{{\"method\":{},\"path\":{},\"upstream\":{}}}\n",
                go_string(parts.method.as_str()),
                go_string(&path),
                go_string(&self.name)
            );
            (StatusCode::OK, body)
        };
        let mut resp = json(status, body);
        if let Ok(v) = HeaderValue::from_str(&self.name) {
            resp.headers_mut().insert("x-upstream", v);
        }
        resp
    }

    /// A uniform draw from [0, 1), like Go's `rand.Float64`.
    fn random(&self) -> f64 {
        crate::health::unit(self.rng.next_u64())
    }

    /// The `/state` document: name, settings and counts.
    pub fn state_json(&self) -> String {
        let (served, failed, held) = self.counts();
        format!(
            "{{\"failed\":{failed},\"held\":{held},\"name\":{},\"served\":{served},\"settings\":{}}}\n",
            go_string(&self.name),
            self.settings().to_json()
        )
    }
}

impl Handler for Server {
    type Body = Full<Bytes>;

    async fn call(
        self: Arc<Self>,
        req: Request<Incoming>,
        _peer: SocketAddr,
    ) -> Response<Self::Body> {
        self.handle(req).await
    }
}

/// The control surface of a [`Server`]: `GET /state` reports it and
/// `PUT /state` patches its settings.
#[derive(Debug, Clone)]
pub struct Control(pub Arc<Server>);

impl Control {
    /// Answers one control request.
    pub async fn handle<B: Body>(&self, req: Request<B>) -> Response<Full<Bytes>> {
        if req.uri().path() != "/state" {
            let mut resp = text(StatusCode::NOT_FOUND, "404 page not found");
            resp.headers_mut().insert(
                "x-content-type-options",
                HeaderValue::from_static("nosniff"),
            );
            return resp;
        }
        let s = &self.0;
        match *req.method() {
            Method::GET => {}
            Method::PUT => {
                let patched = match read_limited(req.into_body(), MAX_CONTROL_BODY + 1).await {
                    Ok(data) => parse_settings(s.settings(), &data),
                    Err(()) => Err("reading the body failed".to_owned()),
                };
                match patched {
                    Ok(next) => s.set(next),
                    Err(message) => {
                        return json_error(StatusCode::BAD_REQUEST, "invalid_argument", &message);
                    }
                }
            }
            _ => {
                let mut resp = json_error(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "method_not_allowed",
                    "use GET or PUT",
                );
                resp.headers_mut()
                    .insert(ALLOW, HeaderValue::from_static("GET, PUT"));
                return resp;
            }
        }
        json(StatusCode::OK, s.state_json())
    }
}

impl Handler for Control {
    type Body = Full<Bytes>;

    async fn call(
        self: Arc<Self>,
        req: Request<Incoming>,
        _peer: SocketAddr,
    ) -> Response<Self::Body> {
        self.handle(req).await
    }
}

/// Reads up to `limit` bytes of `body`, dropping the rest.
async fn read_limited<B: Body>(body: B, limit: usize) -> Result<Vec<u8>, ()> {
    let mut body = std::pin::pin!(body);
    let mut out = Vec::new();
    while out.len() < limit {
        match body.frame().await {
            None => break,
            Some(Err(_)) => return Err(()),
            Some(Ok(frame)) => {
                if let Ok(mut data) = frame.into_data() {
                    use bytes::Buf as _;
                    while data.has_remaining() && out.len() < limit {
                        let chunk = data.chunk();
                        let n = chunk.len().min(limit - out.len());
                        out.extend_from_slice(&chunk[..n]);
                        data.advance(n);
                    }
                }
            }
        }
    }
    Ok(out)
}

fn json(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from(body)));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

fn json_error(status: StatusCode, code: &str, message: &str) -> Response<Full<Bytes>> {
    json(
        status,
        format!(
            "{{\"error\":{{\"code\":{},\"message\":{}}}}}\n",
            go_string(code),
            go_string(message)
        ),
    )
}

/// Go's `http.Error`: plain text and a newline.
fn text(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from(format!("{message}\n"))));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

/// A JSON string as Go's `encoding/json` writes it, HTML-safe: `<`, `>`
/// and `&` become `\u003c`, `\u003e` and `\u0026`, and so do the line and
/// paragraph separators, which JavaScript cannot hold in a string.
pub fn go_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A float64 as Go's `encoding/json` writes it: the shortest decimal that
/// reads back the same, in exponent form below 1e-6 and from 1e21 up.
pub fn go_float(f: f64) -> String {
    let abs = f.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        let e = format!("{f:e}");
        // Go writes a positive exponent with its sign.
        match e.split_once('e') {
            Some((m, x)) if !x.starts_with('-') => format!("{m}e+{x}"),
            _ => e,
        }
    } else {
        format!("{f}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_json_encoding() {
        assert_eq!(go_string("a<b>&c"), r#""a\u003cb\u003e\u0026c""#);
        assert_eq!(
            go_string("\"\\\n\r\t\u{8}\u{c}\u{1}\u{2028}\u{2029}é"),
            r#""\"\\\n\r\t\b\f\u0001\u2028\u2029é""#
        );
        assert_eq!(go_float(0.0), "0");
        assert_eq!(go_float(1.0), "1");
        assert_eq!(go_float(0.5), "0.5");
        assert_eq!(go_float(0.000001), "0.000001");
        assert_eq!(go_float(1.5e-7), "1.5e-7");
        assert_eq!(go_float(1e21), "1e+21");
        assert_eq!(go_float(-2.5e-9), "-2.5e-9");
    }

    #[test]
    fn folding() {
        assert_eq!(fold("Error_Rate"), "ERROR_RATE");
        assert_eq!(fold("blac\u{212a}hole"), "BLACKHOLE");
        assert_eq!(fold("\u{17f}"), "S");
        // A character whose case mapping is more than one character stays.
        assert_eq!(fold("\u{df}"), "\u{df}");
    }

    #[test]
    fn settings_render_like_steer() {
        let s = Settings {
            latency: Duration::from_millis(5),
            error_rate: 0.25,
            blackhole: true,
        };
        assert_eq!(
            s.to_json(),
            r#"{"blackhole":true,"error_rate":0.25,"latency":"5ms"}"#
        );
        assert_eq!(
            Settings::default().to_json(),
            r#"{"blackhole":false,"error_rate":0,"latency":"0s"}"#
        );
    }
}
