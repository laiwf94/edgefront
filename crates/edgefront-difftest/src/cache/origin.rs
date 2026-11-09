//! The origin both cache nodes fetch from.
//!
//! Every answer is a pure function of the request, the logical time and
//! two bits of scenario state that the request log changes: a content
//! version (`/bump`) and whether the origin is failing (`/fail`,
//! `/recover`). Each node gets its own [`Origin`], and the runner applies
//! the same events and the same clock to both, so the two caches see byte
//! for byte the same origin and any difference in what a client gets back
//! comes from the cache.
//!
//! The origin sets `Date` itself from the logical clock, because the
//! nodes compute ages from it and the wall clock would differ from the
//! clock the nodes run on.
//!
//! Routes, by the first path segment:
//!
//! | path                  | answer                                                      |
//! |-----------------------|-------------------------------------------------------------|
//! | `/static/{i}`         | `public, max-age=300`, a strong ETag, tagged `static s{i}`  |
//! | `/page/{i}`           | `max-age=30`, ETag and Last-Modified that move with the version, tagged `page`; 503 while failing |
//! | `/news/{i}`           | `max-age=5, stale-if-error=600`, tagged `news`; 503 while failing |
//! | `/heuristic/{i}`      | only Last-Modified, so freshness is heuristic               |
//! | `/vary/{i}`           | `max-age=60`, `Vary: Accept-Encoding`, body per encoding    |
//! | `/lang/{i}`           | `max-age=60`, `Vary: Accept-Language`, body per language    |
//! | `/nostore/{i}`        | `no-store`                                                  |
//! | `/private/{i}`        | `private, max-age=60`                                       |
//! | `/cookie/{i}`         | `max-age=60` with Set-Cookie                                |
//! | `/nocache/{i}`        | `no-cache` with an ETag, so every use revalidates            |
//! | `/smax/{i}`           | `max-age=0, s-maxage=45`                                    |
//! | `/expires/{i}`        | Expires 40 seconds after Date, no Cache-Control             |
//! | `/big/{i}`            | 96 KiB, over the nodes' object size limit                   |
//! | `/status/{code}/{i}`  | that status, cacheable for 20 seconds when the code allows  |
//! | `/healthz`            | 200; never counted, since health checks run on real time    |
//!
//! Any other method than GET or HEAD gets 200 with a digest of the
//! request body, `no-store`. Every conditional route answers 304 to a
//! matching If-None-Match, or to If-Modified-Since when there is no
//! If-None-Match, as RFC 9110 section 13.2.2 orders them.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use bytes::Bytes;
use edgefront_core::listener::{self, Handler, ServerLimits};
use http::header::{self, HeaderMap, HeaderValue};
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Size of `/big/{i}` bodies.
pub const BIG_BYTES: usize = 96 << 10;

/// Largest `/static/{i}` body.
pub const MAX_STATIC_BYTES: usize = 32 << 10;

/// Scenario state shared by an origin's connections.
#[derive(Debug, Default)]
pub struct State {
    version: AtomicU64,
    failing: AtomicBool,
    now: AtomicU64,
    fetches: AtomicU64,
}

impl State {
    /// A state at logical time `now` (Unix seconds).
    pub fn new(now: u64) -> Self {
        let s = State::default();
        s.now.store(now, Ordering::Relaxed);
        s
    }

    /// Sets the logical time.
    pub fn set_now(&self, now: u64) {
        self.now.store(now, Ordering::Relaxed);
    }

    /// The logical time.
    pub fn now(&self) -> u64 {
        self.now.load(Ordering::Relaxed)
    }

    /// The content version.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// Whether the origin is failing.
    pub fn failing(&self) -> bool {
        self.failing.load(Ordering::Relaxed)
    }

    /// Requests answered, health checks excepted.
    pub fn fetches(&self) -> u64 {
        self.fetches.load(Ordering::Relaxed)
    }

    /// Applies a scenario event: `/bump`, `/fail` or `/recover`.
    pub fn apply(&self, event: &str) -> Result<(), String> {
        match event {
            "/bump" => {
                self.version.fetch_add(1, Ordering::Relaxed);
            }
            "/fail" => self.failing.store(true, Ordering::Relaxed),
            "/recover" => self.failing.store(false, Ordering::Relaxed),
            other => return Err(format!("unknown origin event {other:?}")),
        }
        Ok(())
    }
}

/// The scenario events an `origin` log entry may name.
pub const EVENTS: [&str; 3] = ["/bump", "/fail", "/recover"];

/// What the origin was asked, in the parts its answer depends on.
#[derive(Debug, Clone, Default)]
pub struct Asked<'a> {
    pub method: Method,
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub headers: HeaderMap,
    pub body: &'a [u8],
}

fn http_date(secs: u64) -> String {
    httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(secs))
}

/// Deterministic body bytes for a resource name, `len` long.
pub fn body_bytes(name: &str, len: usize) -> Vec<u8> {
    let seed = Sha256::digest(name.as_bytes());
    (0..len)
        .map(|i| b'a' + (seed[i % seed.len()].wrapping_add((i / 32) as u8) % 26))
        .collect()
}

/// The last path segment as a small number, for per-resource variety.
fn index(path: &str) -> u64 {
    path.rsplit('/')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// The size of `/static/{i}`: the `size` query parameter when it is given,
/// else a size taken from the index.
pub fn static_size(i: u64, query: Option<&str>) -> usize {
    query
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("size=")))
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(match i % 4 {
            0 => 4096,
            1 => 0,
            2 => 1500 + (i as usize * 37) % 2000,
            _ => 8192 + (i as usize * 997) % (MAX_STATIC_BYTES - 8192),
        })
        .min(MAX_STATIC_BYTES)
}

fn etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    let Some(inm) = headers.get(header::IF_NONE_MATCH) else {
        return false;
    };
    let Ok(inm) = inm.to_str() else {
        return false;
    };
    let bare = etag.trim_start_matches("W/");
    inm.split(',')
        .map(str::trim)
        .any(|t| t == "*" || t.trim_start_matches("W/") == bare)
}

/// RFC 9110 section 13.2.2: If-None-Match decides when present, else
/// If-Modified-Since against Last-Modified.
fn not_modified(headers: &HeaderMap, etag: Option<&str>, last_modified: Option<u64>) -> bool {
    if headers.contains_key(header::IF_NONE_MATCH) {
        return etag.is_some_and(|e| etag_matches(headers, e));
    }
    let (Some(lm), Some(ims)) = (last_modified, headers.get(header::IF_MODIFIED_SINCE)) else {
        return false;
    };
    let Some(ims) = ims
        .to_str()
        .ok()
        .and_then(|s| httpdate::parse_http_date(s).ok())
    else {
        return false;
    };
    let ims = ims
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    lm <= ims
}

/// A response the origin builds: status, headers in order, body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub status: StatusCode,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Answer {
    fn new(status: StatusCode, headers: Vec<(&'static str, String)>, body: Vec<u8>) -> Self {
        Answer {
            status,
            headers,
            body,
        }
    }

    /// The same headers with a 304 and no body when the request's
    /// conditionals match.
    fn conditional(
        mut self,
        req: &Asked<'_>,
        etag: Option<&str>,
        last_modified: Option<u64>,
    ) -> Self {
        if not_modified(&req.headers, etag, last_modified) {
            self.status = StatusCode::NOT_MODIFIED;
            self.headers.retain(|(n, _)| *n != "content-type");
            self.body.clear();
        }
        self
    }
}

/// Answers `req` from `state`. A pure function of its inputs, so tests and
/// both nodes' origins agree on every byte.
pub fn answer(state: &State, req: &Asked<'_>) -> Answer {
    let now = state.now();
    let version = state.version();
    let failing = state.failing();
    let mut segments = req.path.trim_start_matches('/').split('/');
    let family = segments.next().unwrap_or("");
    let i = index(req.path);
    let text = |s: String| s.into_bytes();
    let ct = |v: &str| ("content-type", v.to_string());

    if req.method != Method::GET && req.method != Method::HEAD {
        let digest = Sha256::digest(req.body);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        return Answer::new(
            StatusCode::OK,
            vec![("cache-control", "no-store".into()), ct("text/plain")],
            text(format!("{} {} body sha256 {hex}\n", req.method, req.path)),
        );
    }
    match family {
        "static" => {
            let size = static_size(i, req.query);
            let etag = format!("\"s{i}-{size}\"");
            Answer::new(
                StatusCode::OK,
                vec![
                    ("cache-control", "public, max-age=300".into()),
                    ("etag", etag.clone()),
                    ("surrogate-key", format!("static s{i}")),
                    ct("application/octet-stream"),
                ],
                body_bytes(&format!("static/{i}"), size),
            )
            .conditional(req, Some(&etag), None)
        }
        "page" | "news" if failing => Answer::new(
            StatusCode::SERVICE_UNAVAILABLE,
            vec![("retry-after", "5".into()), ct("text/plain")],
            text("origin failing\n".into()),
        ),
        "page" => {
            let etag = format!("\"p{i}-v{version}\"");
            let lm = START_OF_CONTENT + version * 3600 + i;
            Answer::new(
                StatusCode::OK,
                vec![
                    ("cache-control", "max-age=30".into()),
                    ("etag", etag.clone()),
                    ("last-modified", http_date(lm)),
                    ("surrogate-key", "page".into()),
                    ct("text/html"),
                ],
                text(format!("<p>page {i}, version {version}</p>\n")),
            )
            .conditional(req, Some(&etag), Some(lm))
        }
        "news" => Answer::new(
            StatusCode::OK,
            vec![
                ("cache-control", "max-age=5, stale-if-error=600".into()),
                ("surrogate-key", "news".into()),
                ct("text/plain"),
            ],
            text(format!("headlines {i}, edition {version}\n")),
        ),
        "heuristic" => {
            let lm = START_OF_CONTENT - (i % 7) * 86_400;
            Answer::new(
                StatusCode::OK,
                vec![("last-modified", http_date(lm)), ct("text/plain")],
                text(format!("heuristic {i}\n")),
            )
            .conditional(req, None, Some(lm))
        }
        "vary" => {
            let enc = req
                .headers
                .get(header::ACCEPT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            Answer::new(
                StatusCode::OK,
                vec![
                    ("cache-control", "max-age=60".into()),
                    ("vary", "Accept-Encoding".into()),
                    ct("text/plain"),
                ],
                text(format!("vary {i} for encoding [{enc}]\n")),
            )
        }
        "lang" => {
            let lang = req
                .headers
                .get(header::ACCEPT_LANGUAGE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            Answer::new(
                StatusCode::OK,
                vec![
                    ("cache-control", "max-age=60".into()),
                    ("vary", "Accept-Language".into()),
                    (
                        "content-language",
                        if lang.is_empty() { "en" } else { "x" }.into(),
                    ),
                    ct("text/plain"),
                ],
                text(format!("lang {i} for [{lang}]\n")),
            )
        }
        "nostore" => Answer::new(
            StatusCode::OK,
            vec![("cache-control", "no-store".into()), ct("text/plain")],
            text(format!("nostore {i}, version {version}\n")),
        ),
        "private" => Answer::new(
            StatusCode::OK,
            vec![
                ("cache-control", "private, max-age=60".into()),
                ct("text/plain"),
            ],
            text(format!("private {i}\n")),
        ),
        "cookie" => Answer::new(
            StatusCode::OK,
            vec![
                ("cache-control", "max-age=60".into()),
                ("set-cookie", format!("session=s{i}; Path=/; HttpOnly")),
                ct("text/plain"),
            ],
            text(format!("cookie {i}\n")),
        ),
        "nocache" => {
            let etag = format!("\"n{i}-v{version}\"");
            Answer::new(
                StatusCode::OK,
                vec![
                    ("cache-control", "no-cache".into()),
                    ("etag", etag.clone()),
                    ct("text/plain"),
                ],
                text(format!("nocache {i}, version {version}\n")),
            )
            .conditional(req, Some(&etag), None)
        }
        "smax" => Answer::new(
            StatusCode::OK,
            vec![
                ("cache-control", "max-age=0, s-maxage=45".into()),
                ct("text/plain"),
            ],
            text(format!("smax {i}, version {version}\n")),
        ),
        "expires" => Answer::new(
            StatusCode::OK,
            vec![("expires", http_date(now + 40)), ct("text/plain")],
            text(format!("expires {i}, version {version}\n")),
        ),
        "big" => Answer::new(
            StatusCode::OK,
            vec![
                ("cache-control", "max-age=300".into()),
                ct("application/octet-stream"),
            ],
            body_bytes(&format!("big/{i}"), BIG_BYTES),
        ),
        "status" => {
            let code = segments
                .next()
                .and_then(|c| c.parse::<u16>().ok())
                .and_then(|c| StatusCode::from_u16(c).ok())
                .filter(|c| !c.is_informational() && *c != StatusCode::NOT_MODIFIED)
                .unwrap_or(StatusCode::BAD_REQUEST);
            let mut headers = vec![ct("text/plain")];
            match code.as_u16() {
                301 | 308 => {
                    headers.push(("cache-control", "max-age=20".into()));
                    headers.push(("location", format!("/static/{i}")));
                }
                404 | 410 => headers.push(("cache-control", "max-age=20".into())),
                503 => headers.push(("retry-after", "3".into())),
                _ => {}
            }
            let body = if code == StatusCode::NO_CONTENT {
                Vec::new()
            } else {
                text(format!("status {code} {i}\n"))
            };
            Answer::new(code, headers, body)
        }
        "healthz" => Answer::new(StatusCode::OK, vec![ct("text/plain")], text("ok\n".into())),
        _ => Answer::new(
            StatusCode::NOT_FOUND,
            vec![ct("text/plain")],
            text("not found\n".into()),
        ),
    }
}

/// Last-Modified dates count from here: 2026-01-01T00:00:00Z.
pub const START_OF_CONTENT: u64 = 1_767_225_600;

struct Service {
    state: Arc<State>,
}

impl Handler for Service {
    type Body = Full<Bytes>;

    async fn call(
        self: Arc<Self>,
        req: Request<Incoming>,
        _peer: SocketAddr,
    ) -> Response<Full<Bytes>> {
        let (parts, body) = req.into_parts();
        let body = body
            .collect()
            .await
            .map(|b| b.to_bytes())
            .unwrap_or_default();
        let path = parts.uri.path();
        if path != "/healthz" {
            self.state.fetches.fetch_add(1, Ordering::Relaxed);
        }
        let asked = Asked {
            method: parts.method.clone(),
            path,
            query: parts.uri.query(),
            headers: parts.headers.clone(),
            body: &body,
        };
        let a = answer(&self.state, &asked);
        let mut resp = Response::builder().status(a.status);
        resp = resp.header(header::DATE, http_date(self.state.now()));
        for (name, value) in &a.headers {
            resp = resp.header(*name, value);
        }
        let mut resp = resp
            .body(Full::new(Bytes::from(a.body)))
            .expect("origin answers are valid responses");
        if parts.method == Method::HEAD {
            // hyper sends no body for HEAD; keep the length of the GET.
            let len = resp.body().size_hint_exact();
            resp.headers_mut()
                .insert(header::CONTENT_LENGTH, HeaderValue::from(len));
        }
        resp
    }
}

trait ExactLen {
    fn size_hint_exact(&self) -> u64;
}

impl ExactLen for Full<Bytes> {
    fn size_hint_exact(&self) -> u64 {
        hyper::body::Body::size_hint(self).exact().unwrap_or(0)
    }
}

/// An origin listening on a loopback port until dropped.
pub struct Origin {
    pub addr: SocketAddr,
    pub state: Arc<State>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Origin {
    /// Listens on an ephemeral loopback port at logical time `now`.
    pub async fn start(now: u64) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(State::new(now));
        let (stop, stopped) = oneshot::channel::<()>();
        let service = Arc::new(Service {
            state: state.clone(),
        });
        let limits = ServerLimits {
            // The nodes keep pooled connections open between requests
            // that can be far apart in real time.
            idle_timeout: Duration::from_secs(3600),
            drain_timeout: Duration::from_secs(1),
            ..ServerLimits::default()
        };
        let task = tokio::spawn(async move {
            listener::serve(listener, service, limits, async {
                let _ = stopped.await;
            })
            .await;
        });
        Ok(Origin {
            addr,
            state,
            stop: Some(stop),
            task: Some(task),
        })
    }

    /// Stops listening and waits for the connections to close.
    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(path: &'a str, headers: &[(&'static str, &str)]) -> Asked<'a> {
        let (path, query) = match path.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (path, None),
        };
        let mut map = HeaderMap::new();
        for (n, v) in headers {
            map.append(*n, HeaderValue::from_str(v).unwrap());
        }
        Asked {
            method: Method::GET,
            path,
            query,
            headers: map,
            body: b"",
        }
    }

    fn header<'a>(a: &'a Answer, name: &str) -> Option<&'a str> {
        a.headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn static_is_stable_and_conditional() {
        let s = State::new(START_OF_CONTENT);
        let a = answer(&s, &get("/static/3", &[]));
        assert_eq!(a.status, StatusCode::OK);
        assert_eq!(a.body.len(), static_size(3, None));
        s.apply("/bump").unwrap();
        assert_eq!(answer(&s, &get("/static/3", &[])), a);
        let etag = header(&a, "etag").unwrap();
        let nm = answer(&s, &get("/static/3", &[("if-none-match", etag)]));
        assert_eq!(nm.status, StatusCode::NOT_MODIFIED);
        assert!(nm.body.is_empty());
        assert_eq!(header(&nm, "content-type"), None);
        let other = answer(&s, &get("/static/3", &[("if-none-match", "\"x\", \"y\"")]));
        assert_eq!(other.status, StatusCode::OK);
        let star = answer(&s, &get("/static/3", &[("if-none-match", "*")]));
        assert_eq!(star.status, StatusCode::NOT_MODIFIED);
        let weak = format!("W/{etag}");
        let weak = answer(&s, &get("/static/3", &[("if-none-match", &weak)]));
        assert_eq!(weak.status, StatusCode::NOT_MODIFIED);
    }

    #[test]
    fn static_sizes() {
        assert_eq!(static_size(0, None), 4096);
        assert_eq!(static_size(1, None), 0);
        assert!((1500..3500).contains(&static_size(2, None)));
        assert!((8192..MAX_STATIC_BYTES).contains(&static_size(3, None)));
        assert_eq!(static_size(3, Some("a=1&size=10")), 10);
        assert_eq!(static_size(3, Some("size=999999999")), MAX_STATIC_BYTES);
        assert_eq!(static_size(0, Some("size=x")), 4096);
    }

    #[test]
    fn pages_move_with_the_version_and_fail() {
        let s = State::new(START_OF_CONTENT + 100);
        let v0 = answer(&s, &get("/page/2", &[]));
        let etag = header(&v0, "etag").unwrap().to_string();
        let lm = header(&v0, "last-modified").unwrap().to_string();
        assert_eq!(
            answer(&s, &get("/page/2", &[("if-none-match", &etag)])).status,
            StatusCode::NOT_MODIFIED
        );
        assert_eq!(
            answer(&s, &get("/page/2", &[("if-modified-since", &lm)])).status,
            StatusCode::NOT_MODIFIED
        );
        // If-None-Match wins over If-Modified-Since.
        assert_eq!(
            answer(
                &s,
                &get(
                    "/page/2",
                    &[("if-none-match", "\"old\""), ("if-modified-since", &lm)]
                )
            )
            .status,
            StatusCode::OK
        );
        assert_eq!(
            answer(&s, &get("/page/2", &[("if-modified-since", "garbage")])).status,
            StatusCode::OK
        );
        s.apply("/bump").unwrap();
        let v1 = answer(&s, &get("/page/2", &[("if-none-match", &etag)]));
        assert_eq!(v1.status, StatusCode::OK);
        assert_ne!(v1.body, v0.body);
        assert_eq!(
            answer(&s, &get("/page/2", &[("if-modified-since", &lm)])).status,
            StatusCode::OK
        );
        s.apply("/fail").unwrap();
        assert!(s.failing());
        assert_eq!(
            answer(&s, &get("/page/2", &[])).status,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            answer(&s, &get("/news/2", &[])).status,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(answer(&s, &get("/static/2", &[])).status, StatusCode::OK);
        assert_eq!(answer(&s, &get("/healthz", &[])).status, StatusCode::OK);
        s.apply("/recover").unwrap();
        assert_eq!(answer(&s, &get("/news/2", &[])).status, StatusCode::OK);
        assert!(s.apply("/explode").is_err());
    }

    #[test]
    fn every_family_answers() {
        let s = State::new(START_OF_CONTENT);
        for (path, status, cc) in [
            ("/heuristic/3", 200, None),
            ("/vary/1", 200, Some("max-age=60")),
            ("/lang/1", 200, Some("max-age=60")),
            ("/nostore/1", 200, Some("no-store")),
            ("/private/1", 200, Some("private, max-age=60")),
            ("/cookie/1", 200, Some("max-age=60")),
            ("/nocache/1", 200, Some("no-cache")),
            ("/smax/1", 200, Some("max-age=0, s-maxage=45")),
            ("/expires/1", 200, None),
            ("/big/1", 200, Some("max-age=300")),
            ("/status/404/1", 404, Some("max-age=20")),
            ("/status/301/1", 301, Some("max-age=20")),
            ("/status/503/1", 503, None),
            ("/status/204/1", 204, None),
            ("/status/500/1", 500, None),
            ("/status/abc/1", 400, None),
            ("/status/304/1", 400, None),
            ("/status/100/1", 400, None),
            ("/healthz", 200, None),
            ("/nowhere", 404, None),
        ] {
            let a = answer(&s, &get(path, &[]));
            assert_eq!(a.status.as_u16(), status, "{path}");
            assert_eq!(header(&a, "cache-control"), cc, "{path}");
        }
        assert_eq!(answer(&s, &get("/big/1", &[])).body.len(), BIG_BYTES);
        assert!(answer(&s, &get("/status/204/1", &[])).body.is_empty());
        assert_eq!(
            header(&answer(&s, &get("/expires/1", &[])), "expires").unwrap(),
            http_date(START_OF_CONTENT + 40)
        );
        let lm = header(&answer(&s, &get("/heuristic/3", &[])), "last-modified")
            .unwrap()
            .to_string();
        assert_eq!(
            answer(&s, &get("/heuristic/3", &[("if-modified-since", &lm)])).status,
            StatusCode::NOT_MODIFIED
        );
        let gz = answer(&s, &get("/vary/1", &[("accept-encoding", "gzip")]));
        assert_ne!(gz.body, answer(&s, &get("/vary/1", &[])).body);
        let fr = answer(&s, &get("/lang/1", &[("accept-language", "fr")]));
        assert_eq!(header(&fr, "content-language"), Some("x"));
    }

    #[test]
    fn unsafe_methods_digest_the_body() {
        let s = State::new(0);
        let mut req = get("/page/1", &[]);
        req.method = Method::POST;
        req.body = b"hello";
        let a = answer(&s, &req);
        assert_eq!(header(&a, "cache-control"), Some("no-store"));
        let text = String::from_utf8(a.body).unwrap();
        assert!(text.contains("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"));
    }

    #[test]
    fn bodies_are_deterministic_letters() {
        let a = body_bytes("x", 100);
        assert_eq!(a, body_bytes("x", 100));
        assert_ne!(a, body_bytes("y", 100));
        assert!(a.iter().all(u8::is_ascii_lowercase));
    }

    #[tokio::test]
    async fn serves_over_http_and_counts_fetches() {
        let origin = Origin::start(START_OF_CONTENT).await.unwrap();
        let client = edgefront_core::http1::Client::<Full<Bytes>>::new(&Default::default());
        let send = |method: Method, path: &str| {
            let req = Request::builder()
                .method(method)
                .uri(format!("http://{}{path}", origin.addr))
                .body(Full::new(Bytes::new()))
                .unwrap();
            client.send(req)
        };
        let resp = send(Method::GET, "/healthz").await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["date"], http_date(START_OF_CONTENT));
        assert_eq!(origin.state.fetches(), 0);
        let resp = send(Method::HEAD, "/static/0").await.unwrap();
        assert_eq!(resp.headers()["content-length"], "4096");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(body.is_empty());
        origin.state.set_now(START_OF_CONTENT + 61);
        let resp = send(Method::GET, "/static/0").await.unwrap();
        assert_eq!(resp.headers()["date"], http_date(START_OF_CONTENT + 61));
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.len(), 4096);
        assert_eq!(origin.state.fetches(), 2);
        origin.stop().await;
    }
}
