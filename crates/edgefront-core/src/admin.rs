//! The admin listener's building blocks: a router keyed by method and
//! exact path, bearer-token checks, JSON error bodies and bounded request
//! bodies.
//!
//! A product registers its own routes on a [`Router`] over its shared
//! state and serves the router like any other [`Handler`]. Requests for a
//! path no route serves get 404 and requests for a served path with the
//! wrong method get 405, both as `{"error": ...}` JSON. Admin traffic is
//! rare, so each route's future is boxed; the data path never goes
//! through here.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderValue};
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use sha2::{Digest, Sha256};

use crate::body::{Body, full, text};
use crate::listener::Handler;

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
}
