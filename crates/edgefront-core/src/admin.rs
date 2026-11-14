//! The admin listener's building blocks: a router keyed by method and
//! exact path, bearer-token checks, JSON error bodies and bounded request
//! bodies; and [`Api`], steer's operator API ported from its
//! `internal/admin`.
//!
//! A product registers its own routes on a [`Router`] over its shared
//! state and serves the router like any other [`Handler`]. Requests for a
//! path no route serves get 404 and requests for a served path with the
//! wrong method get 405, both as `{"error": ...}` JSON. Admin traffic is
//! rare, so each route's future is boxed; the data path never goes
//! through here.
//!
//! [`Api`] answers steer's endpoints with steer's JSON, byte for byte:
//!
//! | route | auth | answer |
//! |---|---|---|
//! | `GET /healthz` | open | `{"status": "ok"}` |
//! | `GET /readyz` | open | `{"status": "ready"}`, or 503 `{"status": "draining"}` |
//! | `GET /metrics` | token | the Prometheus text |
//! | `GET /clusters` | token | [`ProxyState`]: clusters, balancers, endpoints |
//! | `POST /reload` | token | `{"config_version": n, "status": "reloaded"}`, or 400 `invalid_config` |
//!
//! With a token configured, the token routes need `Authorization: Bearer
//! <token>`; the check hashes both sides with SHA-256 and compares the
//! digests without an early exit, so neither the token's length nor a
//! matching prefix shows in the timing. Authentication comes before the
//! method check, so an unauthenticated caller learns nothing about a
//! route. Errors are `{"error": {"code", "message", "request_id"}}`.
//! Paths are matched the way Go's `ServeMux` matches them: percent-decoded,
//! and redirected with 307 to their cleaned form when they hold `//`, `.`
//! or `..` segments.

pub mod go_json;
mod state;

pub use state::{BalancerState, ClusterState, EndpointState, ProxyState};

use std::fmt;
use std::fmt::Write as _;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderValue};
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use sha2::{Digest, Sha256};

use crate::body::{Body, full, text};
use crate::listener::Handler;
use crate::reqid;
use go_json::Json;

/// A `Cache-Control: no-store` JSON response.
pub fn json(status: StatusCode, value: &serde_json::Value) -> Response<Body> {
    let mut resp = Response::new(full(Bytes::from(value.to_string())));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// A JSON error: `{"error": message}`.
pub fn problem(status: StatusCode, message: &str) -> Response<Body> {
    json(status, &serde_json::json!({ "error": message }))
}

/// A request the admin API turns away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    pub status: StatusCode,
    pub message: &'static str,
}

impl Refusal {
    /// The refusal as a JSON error; a 401 also carries
    /// `WWW-Authenticate: Bearer`.
    pub fn into_response(self) -> Response<Body> {
        let mut resp = problem(self.status, self.message);
        if self.status == StatusCode::UNAUTHORIZED {
            resp.headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        resp
    }
}

/// Reads a request body of at most `limit` bytes.
pub async fn read_body(body: Incoming, limit: usize) -> Result<Bytes, Refusal> {
    match Limited::new(body, limit).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(e) if e.is::<http_body_util::LengthLimitError>() => Err(Refusal {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: "body too large",
        }),
        Err(_) => Err(Refusal {
            status: StatusCode::BAD_REQUEST,
            message: "could not read body",
        }),
    }
}

/// An expected bearer token, kept only as its SHA-256 digest.
#[derive(Clone)]
pub struct BearerToken([u8; 32]);

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BearerToken(<set>)")
    }
}

impl BearerToken {
    pub fn new(token: &str) -> Self {
        BearerToken(Sha256::digest(token.as_bytes()).into())
    }

    /// Whether `given` is the token. The comparison has no data-dependent
    /// early exit, and hashing first also hides the expected length.
    pub fn matches(&self, given: &[u8]) -> bool {
        let given = Sha256::digest(given);
        self.0
            .iter()
            .zip(given.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }

    /// Checks `Authorization: Bearer <token>`.
    pub fn check(&self, headers: &HeaderMap) -> Result<(), Refusal> {
        let given = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.as_bytes().strip_prefix(b"Bearer "));
        match given {
            Some(token) if self.matches(token) => Ok(()),
            _ => Err(Refusal {
                status: StatusCode::UNAUTHORIZED,
                message: "missing or wrong bearer token",
            }),
        }
    }
}

/// What a route returns.
pub type RouteFuture = Pin<Box<dyn Future<Output = Response<Body>> + Send>>;

type RouteFn<S> = Box<dyn Fn(Arc<S>, Request<Incoming>, SocketAddr) -> RouteFuture + Send + Sync>;

struct Route<S> {
    /// `None` serves every method.
    method: Option<Method>,
    path: &'static str,
    handler: RouteFn<S>,
}

/// Routes admin requests over shared state `S`.
pub struct Router<S> {
    state: Arc<S>,
    routes: Vec<Route<S>>,
}

impl<S: Send + Sync + 'static> Router<S> {
    pub fn new(state: Arc<S>) -> Self {
        Router {
            state,
            routes: Vec::new(),
        }
    }

    /// The state routes are called with.
    pub fn state(&self) -> &Arc<S> {
        &self.state
    }

    fn add<F, Fut>(mut self, method: Option<Method>, path: &'static str, handler: F) -> Self
    where
        F: Fn(Arc<S>, Request<Incoming>, SocketAddr) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Response<Body>> + Send + 'static,
    {
        assert!(
            path.starts_with('/'),
            "route path {path:?} must start with /"
        );
        assert!(
            !self.routes.iter().any(|r| r.path == path
                && (r.method.is_none() || method.is_none() || r.method == method)),
            "route {method:?} {path} is already registered"
        );
        self.routes.push(Route {
            method,
            path,
            handler: Box::new(move |state, req, peer| Box::pin(handler(state, req, peer))),
        });
        self
    }

    /// Serves `method` requests for exactly `path`.
    pub fn route<F, Fut>(self, method: Method, path: &'static str, handler: F) -> Self
    where
        F: Fn(Arc<S>, Request<Incoming>, SocketAddr) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Response<Body>> + Send + 'static,
    {
        self.add(Some(method), path, handler)
    }

    /// Serves every method for exactly `path`; the handler decides.
    pub fn any<F, Fut>(self, path: &'static str, handler: F) -> Self
    where
        F: Fn(Arc<S>, Request<Incoming>, SocketAddr) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Response<Body>> + Send + 'static,
    {
        self.add(None, path, handler)
    }

    /// Serves `GET /healthz` with a plain `ok`.
    pub fn health(self) -> Self {
        self.route(Method::GET, "/healthz", |_, _, _| async {
            text(StatusCode::OK, "ok\n")
        })
    }

    /// Answers one request.
    pub fn dispatch(&self, req: Request<Incoming>, peer: SocketAddr) -> RouteFuture {
        let path = req.uri().path();
        let mut path_known = false;
        for route in &self.routes {
            if route.path != path {
                continue;
            }
            path_known = true;
            if route.method.as_ref().is_none_or(|m| m == req.method()) {
                return (route.handler)(self.state.clone(), req, peer);
            }
        }
        let resp = if path_known {
            problem(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
        } else {
            problem(StatusCode::NOT_FOUND, "not found")
        };
        Box::pin(std::future::ready(resp))
    }
}

impl<S: Send + Sync + 'static> Handler for Router<S> {
    type Body = Body;

    async fn call(self: Arc<Self>, req: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
        self.dispatch(req, peer).await
    }
}

/// The shortest admin token [`Api`] accepts, in bytes.
pub const MIN_TOKEN_LENGTH: usize = 16;

/// What [`Api`] needs from the process it administers.
pub trait Control: Send + Sync + 'static {
    /// False while the process drains, so `/readyz` turns load balancers
    /// away.
    fn ready(&self) -> bool;

    /// The live state `/clusters` reports.
    fn state(&self) -> ProxyState;

    /// Re-reads the configuration and applies it, returning the new
    /// configuration version, or why the configuration was refused.
    fn reload(&self) -> impl Future<Output = Result<u64, String>> + Send;

    /// Appends the Prometheus text exposition `/metrics` serves.
    fn metrics(&self, out: &mut String);
}

/// An admin token shorter than [`MIN_TOKEN_LENGTH`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenTooShort;

impl fmt::Display for TokenTooShort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "admin: token must be at least {MIN_TOKEN_LENGTH} characters"
        )
    }
}

impl std::error::Error for TokenTooShort {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    Healthz,
    Readyz,
    Metrics,
    Clusters,
    Reload,
}

/// steer's routes: path, the one method served, whether the token guards
/// it, and what answers.
const ROUTES: [(&str, Method, bool, Endpoint); 5] = [
    ("/healthz", Method::GET, false, Endpoint::Healthz),
    ("/readyz", Method::GET, false, Endpoint::Readyz),
    ("/metrics", Method::GET, true, Endpoint::Metrics),
    ("/clusters", Method::GET, true, Endpoint::Clusters),
    ("/reload", Method::POST, true, Endpoint::Reload),
];

/// steer's operator API over a [`Control`].
pub struct Api<C> {
    control: Arc<C>,
    token: Option<BearerToken>,
}

impl<C> fmt::Debug for Api<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Api")
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl<C: Control> Api<C> {
    /// The API over `control`. An empty `token` turns authentication off,
    /// which the caller allows only on a loopback address, as steer did;
    /// any other token must be at least [`MIN_TOKEN_LENGTH`] bytes.
    pub fn new(control: Arc<C>, token: &str) -> Result<Api<C>, TokenTooShort> {
        let token = match token.len() {
            0 => None,
            n if n < MIN_TOKEN_LENGTH => return Err(TokenTooShort),
            _ => Some(BearerToken::new(token)),
        };
        Ok(Api { control, token })
    }

    /// Whether a token guards the API.
    pub fn authenticates(&self) -> bool {
        self.token.is_some()
    }

    /// Answers one request. The body is never read.
    pub async fn handle<B>(&self, req: Request<B>, remote: SocketAddr) -> Response<Body> {
        let connect = req.method() == Method::CONNECT;
        let mut resp = self.route(req, remote).await;
        if connect {
            // Go's server closes the connection after a CONNECT.
            resp.headers_mut()
                .insert(header::CONNECTION, HeaderValue::from_static("close"));
        }
        resp
    }

    async fn route<B>(&self, req: Request<B>, remote: SocketAddr) -> Response<Body> {
        let raw = req.uri().path();
        if raw == "*" {
            // Go's server answers `OPTIONS *` itself and refuses `*` for
            // any other method.
            let mut resp = Response::new(full(Bytes::new()));
            if req.method() == Method::OPTIONS {
                resp.headers_mut()
                    .insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
            } else {
                *resp.status_mut() = StatusCode::BAD_REQUEST;
                resp.headers_mut()
                    .insert(header::CONNECTION, HeaderValue::from_static("close"));
            }
            return resp;
        }
        let Some(decoded) = unescape_path(raw) else {
            // Go's server refuses a request target it cannot parse before
            // any handler runs.
            let mut resp = plain(StatusCode::BAD_REQUEST, "400 Bad Request");
            resp.headers_mut()
                .insert(header::CONNECTION, HeaderValue::from_static("close"));
            return resp;
        };
        // Go's `ServeMux` cleans the escaped path, which is the path as
        // sent when every byte in it may stand unescaped, and matches the
        // unescaped segments of the result. A CONNECT is not cleaned.
        let path = if req.method() == Method::CONNECT {
            decoded
        } else {
            let escaped = if valid_encoded(raw.as_bytes()) {
                raw.as_bytes().to_vec()
            } else {
                escape_path(&decoded).into_bytes()
            };
            let clean = clean_path(&escaped);
            if clean != escaped {
                return redirect(&clean, req.uri().query(), req.method());
            }
            unescape_path(&String::from_utf8_lossy(&clean)).unwrap_or_default()
        };
        let Some(&(pattern, ref method, auth, endpoint)) =
            ROUTES.iter().find(|r| r.0.as_bytes() == path.as_slice())
        else {
            if !path.starts_with(b"/") {
                // Only a CONNECT gets here, and no pattern can match it.
                let mut resp = plain(StatusCode::NOT_FOUND, "404 page not found\n");
                resp.headers_mut().insert(
                    header::X_CONTENT_TYPE_OPTIONS,
                    HeaderValue::from_static("nosniff"),
                );
                return resp;
            }
            // steer's catch-all writes its error outside the logged
            // wrapper, so the ID is always new and nothing is logged.
            let id = reqid::new_id();
            let mut resp = error(&id, StatusCode::NOT_FOUND, "not_found", "no such endpoint");
            resp.headers_mut().insert(reqid::HEADER, id);
            return resp;
        };

        let start = Instant::now();
        let id = reqid::from_request(req.headers());
        let mut resp = if auth && !self.authorized(req.headers()) {
            let mut r = error(
                &id,
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                "missing or invalid admin token",
            );
            r.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"steer-admin\""),
            );
            r
        } else if req.method() != method {
            let mut r = error(
                &id,
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                &format!("use {method}"),
            );
            r.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static(method.as_str()));
            r
        } else {
            self.serve(endpoint, &id).await
        };
        let h = resp.headers_mut();
        h.insert(reqid::HEADER, id.clone());
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        h.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        tracing::info!(
            request_id = id.to_str().unwrap_or_default(),
            method = req.method().as_str(),
            path = pattern,
            status = resp.status().as_u16(),
            remote = %remote,
            duration_ms = start.elapsed().as_micros() as f64 / 1000.0,
            "admin request"
        );
        resp
    }

    /// Whether `Authorization` carries the token, or no token is set.
    fn authorized(&self, headers: &HeaderMap) -> bool {
        let Some(token) = &self.token else {
            return true;
        };
        // Go's Header.Get: the first value only.
        match headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.as_bytes().strip_prefix(b"Bearer "))
        {
            Some(given) if !given.is_empty() => token.matches(given),
            _ => false,
        }
    }

    async fn serve(&self, endpoint: Endpoint, id: &HeaderValue) -> Response<Body> {
        match endpoint {
            Endpoint::Healthz => status_doc(StatusCode::OK, "ok"),
            Endpoint::Readyz if self.control.ready() => status_doc(StatusCode::OK, "ready"),
            Endpoint::Readyz => status_doc(StatusCode::SERVICE_UNAVAILABLE, "draining"),
            Endpoint::Metrics => {
                let mut out = String::new();
                self.control.metrics(&mut out);
                let mut resp = Response::new(full(Bytes::from(out)));
                resp.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; version=0.0.4"),
                );
                resp
            }
            Endpoint::Clusters => go_json(StatusCode::OK, &self.control.state().to_json()),
            Endpoint::Reload => match self.control.reload().await {
                Ok(version) => go_json(
                    StatusCode::OK,
                    &Json::map([
                        ("status", Json::str("reloaded")),
                        ("config_version", Json::Uint(version)),
                    ]),
                ),
                Err(e) => {
                    tracing::warn!(error = %e, "reload rejected");
                    error(id, StatusCode::BAD_REQUEST, "invalid_config", &e)
                }
            },
        }
    }
}

impl<C: Control> Handler for Api<C> {
    type Body = Body;

    async fn call(self: Arc<Self>, req: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
        self.handle(req, peer).await
    }
}

/// steer's `writeJSON`: the indented document as `application/json`. A
/// document Go cannot encode leaves the body empty after the status, as
/// Go's encoder fails before writing.
fn go_json(status: StatusCode, doc: &Json) -> Response<Body> {
    let body = doc.indented().unwrap_or_default();
    let mut resp = Response::new(full(Bytes::from(body)));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

fn status_doc(status: StatusCode, word: &str) -> Response<Body> {
    go_json(status, &Json::map([("status", Json::str(word))]))
}

/// steer's `writeError`.
fn error(id: &HeaderValue, status: StatusCode, code: &str, message: &str) -> Response<Body> {
    let id = String::from_utf8_lossy(id.as_bytes());
    let doc = Json::map([(
        "error",
        Json::map([
            ("code", Json::str(code)),
            ("message", Json::str(message)),
            ("request_id", Json::str(id)),
        ]),
    )]);
    go_json(status, &doc)
}

/// A plain-text answer as Go's server writes one outside any handler.
fn plain(status: StatusCode, message: &'static str) -> Response<Body> {
    let mut resp = Response::new(full(message));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

/// Go's `ServeMux` redirect to the cleaned escaped path, a 307 as
/// `http.Redirect` writes it.
fn redirect(escaped: &[u8], query: Option<&str>, method: &Method) -> Response<Body> {
    // The path holds only bytes `valid_encoded` allows, all ASCII.
    let mut location = String::from_utf8_lossy(escaped).into_owned();
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        location.push('?');
        location.push_str(q);
    }
    let body = if method == Method::GET {
        format!(
            "<a href=\"{}\">Temporary Redirect</a>.\n\n",
            html_escape(&location)
        )
    } else {
        String::new()
    };
    let mut resp = Response::new(full(Bytes::from(body)));
    *resp.status_mut() = StatusCode::TEMPORARY_REDIRECT;
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::try_from(location) {
        h.insert(header::LOCATION, v);
    }
    if method == Method::GET || method == Method::HEAD {
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
    }
    resp
}

/// Decodes `%XX` escapes as Go's URL parser does for a path; `None` for a
/// `%` not followed by two hex digits.
fn unescape_path(raw: &str) -> Option<Vec<u8>> {
    let b = raw.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hi = hex(*b.get(i + 1)?)?;
            let lo = hex(*b.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Go's `ServeMux` `cleanPath`: rooted `path.Clean`, keeping a trailing
/// slash. `..` and `.` count only as they are written, not escaped.
fn clean_path(p: &[u8]) -> Vec<u8> {
    let mut segs: Vec<&[u8]> = Vec::new();
    for seg in p.split(|&c| c == b'/') {
        match seg {
            b"" | b"." => {}
            b".." => {
                segs.pop();
            }
            s => segs.push(s),
        }
    }
    let mut out = Vec::with_capacity(p.len() + 1);
    for s in &segs {
        out.push(b'/');
        out.extend_from_slice(s);
    }
    if segs.is_empty() || p.last() == Some(&b'/') {
        out.push(b'/');
    }
    out
}

/// Go's `validEncoded` for a path: whether every byte may stand as it is
/// in an escaped path.
fn valid_encoded(p: &[u8]) -> bool {
    p.iter()
        .all(|&c| c.is_ascii_alphanumeric() || b"-_.~!$&'()*+,;=:@[]%/".contains(&c))
}

/// Go's `escape` of a path, for a path whose escaped form is not valid.
fn escape_path(p: &[u8]) -> String {
    let mut out = String::with_capacity(p.len());
    for &c in p {
        if c.is_ascii_alphanumeric() || b"-_.~$&+,/:;=@".contains(&c) {
            out.push(char::from(c));
        } else {
            let _ = write!(out, "%{c:02X}");
        }
    }
    out
}

/// Go's `html.EscapeString`.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body(resp: Response<Body>) -> Bytes {
        resp.into_body().collect().await.unwrap().to_bytes()
    }

    #[tokio::test]
    async fn json_errors() {
        let resp = problem(StatusCode::BAD_REQUEST, "bad \"thing\"");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(body(resp).await, r#"{"error":"bad \"thing\""}"#);
    }

    #[tokio::test]
    async fn refusals() {
        let r = Refusal {
            status: StatusCode::UNAUTHORIZED,
            message: "no",
        }
        .into_response();
        assert_eq!(r.headers()[header::WWW_AUTHENTICATE], "Bearer");
        assert_eq!(body(r).await, r#"{"error":"no"}"#);
        let r = Refusal {
            status: StatusCode::FORBIDDEN,
            message: "off",
        }
        .into_response();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(r.headers().get(header::WWW_AUTHENTICATE).is_none());
    }

    #[test]
    fn bearer_tokens() {
        let t = BearerToken::new("token");
        assert!(t.matches(b"token"));
        assert!(!t.matches(b"token "));
        assert!(!t.matches(b""));
        assert_eq!(format!("{t:?}"), "BearerToken(<set>)");
        let mut h = HeaderMap::new();
        assert_eq!(t.check(&h).unwrap_err().status, StatusCode::UNAUTHORIZED);
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("token"));
        assert!(t.check(&h).is_err());
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer nope"),
        );
        assert!(t.check(&h).is_err());
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer token"),
        );
        assert!(t.check(&h).is_ok());
    }

    #[test]
    #[should_panic(expected = "already registered")]
    fn duplicate_routes_panic() {
        let _ = Router::new(Arc::new(()))
            .health()
            .any("/healthz", |_, _, _| async { text(StatusCode::OK, "") });
    }

    #[test]
    #[should_panic(expected = "must start with /")]
    fn relative_paths_panic() {
        let _ =
            Router::new(Arc::new(())).any("healthz", |_, _, _| async { text(StatusCode::OK, "") });
    }

    #[test]
    fn methods_can_share_a_path() {
        let r = Router::new(Arc::new(7))
            .route(Method::GET, "/x", |_, _, _| async {
                text(StatusCode::OK, "")
            })
            .route(Method::POST, "/x", |_, _, _| async {
                text(StatusCode::OK, "")
            });
        assert_eq!(r.routes.len(), 2);
        assert_eq!(**r.state(), 7);
    }

    struct Fixed {
        refuse: bool,
    }

    impl Control for Fixed {
        fn ready(&self) -> bool {
            true
        }

        fn state(&self) -> ProxyState {
            ProxyState {
                config_version: 1,
                loaded_at: go_json::unix(0, 0),
                clusters: vec![],
            }
        }

        async fn reload(&self) -> Result<u64, String> {
            if self.refuse { Err("no".into()) } else { Ok(2) }
        }

        fn metrics(&self, _: &mut String) {}
    }

    fn peer() -> SocketAddr {
        "127.0.0.1:1".parse().unwrap()
    }

    async fn call(api: &Api<Fixed>, method: &str, uri: &str) -> Response<Body> {
        let req = Request::builder().method(method).uri(uri).body(()).unwrap();
        api.handle(req, peer()).await
    }

    #[test]
    fn tokens_must_be_long_enough() {
        let f = || Arc::new(Fixed { refuse: false });
        let err = Api::new(f(), "short").unwrap_err();
        assert_eq!(
            err.to_string(),
            "admin: token must be at least 16 characters"
        );
        assert!(Api::new(f(), &"x".repeat(15)).is_err());
        let api = Api::new(f(), &"x".repeat(16)).unwrap();
        assert!(api.authenticates());
        assert_eq!(
            format!("{api:?}"),
            "Api { token: Some(BearerToken(<set>)), .. }"
        );
        let open = Api::new(f(), "").unwrap();
        assert!(!open.authenticates());
        assert!(open.authorized(&HeaderMap::new()));
    }

    #[tokio::test]
    async fn requests_go_does_not_route() {
        let api = Api::new(Arc::new(Fixed { refuse: false }), "").unwrap();
        // A CONNECT is not cleaned and, with no path, matches nothing.
        let r = call(&api, "CONNECT", "example.com:443").await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(r.headers()[header::CONNECTION], "close");
        assert_eq!(r.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(body(r).await, "404 page not found\n");
        // A bad escape is refused as Go's URL parser refuses it.
        for uri in ["/%", "/%4", "/%g0", "/a%0z"] {
            let r = call(&api, "GET", uri).await;
            assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(r.headers()[header::CONNECTION], "close");
        }
        let r = call(&api, "OPTIONS", "*").await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CONTENT_LENGTH], "0");
        let r = call(&api, "GET", "*").await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(r.headers()[header::CONNECTION], "close");
        assert_eq!(body(r).await, "");
        // A CONNECT with a path is routed without cleaning, and the
        // connection closes after it.
        let r = call(&api, "CONNECT", "/healthz").await;
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(r.headers()[header::CONNECTION], "close");
        let r = call(&api, "CONNECT", "//healthz").await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert!(body(r).await.starts_with(b"{\n  \"error\""));
    }

    #[tokio::test]
    async fn refused_reloads_say_why() {
        let api = Api::new(Arc::new(Fixed { refuse: true }), "").unwrap();
        let r = call(&api, "POST", "/reload").await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let b = body(r).await;
        assert!(b.starts_with(
            b"{\n  \"error\": {\n    \"code\": \"invalid_config\",\n    \"message\": \"no\""
        ));
        let r = call(&api, "GET", "/metrics").await;
        assert_eq!(body(r).await, "");
    }

    #[test]
    fn go_path_helpers() {
        for (p, want) in [
            ("/", "/"),
            ("//", "/"),
            ("/a/./b/", "/a/b/"),
            ("/a/../../b", "/b"),
            ("/a/..", "/"),
            ("/a/../", "/"),
            ("/..", "/"),
            ("/a//b//", "/a/b/"),
            ("*", "/*"),
            ("/%2E%2E/x", "/%2E%2E/x"),
        ] {
            assert_eq!(clean_path(p.as_bytes()), want.as_bytes(), "{p}");
        }
        assert_eq!(unescape_path("/a%2fb%41").unwrap(), b"/a/bA");
        assert_eq!(unescape_path("/%"), None);
        assert!(valid_encoded(b"/a-b_c.d~!$&'()*+,;=:@[]%20/"));
        for c in [
            b'"', b'<', b'>', b'{', b'|', b'^', b'`', b' ', b'?', b'#', 0x80,
        ] {
            assert!(!valid_encoded(&[b'/', c]), "{c}");
        }
        assert_eq!(escape_path(b"/a b\"<\x80!"), "/a%20b%22%3C%80%21");
        assert_eq!(
            html_escape("<a href='x'>\"&\"</a>"),
            "&lt;a href=&#39;x&#39;&gt;&#34;&amp;&#34;&lt;/a&gt;"
        );
    }

    #[tokio::test]
    async fn invalid_escaped_paths_are_escaped_before_cleaning() {
        let api = Api::new(Arc::new(Fixed { refuse: false }), "").unwrap();
        // `"` may not stand unescaped, so Go cleans the escaped form.
        let r = call(&api, "GET", "/a/../%22x").await;
        assert_eq!(r.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(r.headers()[header::LOCATION], "/%22x");
        let r = call(&api, "POST", "/a/../reload").await;
        assert!(r.headers().get(header::CONTENT_TYPE).is_none());
        assert_eq!(body(r).await, "");
    }
}
