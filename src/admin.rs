//! The admin listener: health, metrics and purges.
//!
//! It binds to localhost by default. `POST /purge` needs the admin token as
//! a bearer token; without a configured token the purge API is off.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use http::header::{self, HeaderValue};
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use sha2::{Digest, Sha256};

use crate::proxy::Proxy;
use crate::purge::{self, MAX_PURGE_BODY};
use crate::respond::{Body, full, text};
use crate::server::Handler;

pub struct Admin {
    proxy: Arc<Proxy>,
    token: Option<[u8; 32]>,
}

/// Compares two byte strings without a data-dependent early exit. Hashing
/// first also hides the length of the expected value.
fn same_secret(expected: &[u8; 32], given: &[u8]) -> bool {
    let given = Sha256::digest(given);
    expected
        .iter()
        .zip(given.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn json(status: StatusCode, value: &serde_json::Value) -> Response<Body> {
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

fn problem(status: StatusCode, message: &str) -> Response<Body> {
    json(status, &serde_json::json!({ "error": message }))
}

/// A request the admin API turns away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub status: StatusCode,
    pub message: &'static str,
}

impl Refusal {
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
pub(crate) async fn read_body(body: Incoming, limit: usize) -> Result<Bytes, Refusal> {
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

impl Admin {
    pub fn new(proxy: Arc<Proxy>, token: Option<&str>) -> Self {
        Admin {
            proxy,
            token: token.map(|t| Sha256::digest(t.as_bytes()).into()),
        }
    }

    fn authorized(&self, req: &Request<Incoming>) -> Result<(), Refusal> {
        let Some(expected) = &self.token else {
            return Err(Refusal {
                status: StatusCode::FORBIDDEN,
                message: "purge API is disabled: set CACHET_ADMIN_TOKEN",
            });
        };
        let given = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.as_bytes().strip_prefix(b"Bearer "));
        match given {
            Some(token) if same_secret(expected, token) => Ok(()),
            _ => Err(Refusal {
                status: StatusCode::UNAUTHORIZED,
                message: "missing or wrong bearer token",
            }),
        }
    }

    async fn purge(&self, req: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
        if let Err(refusal) = self.authorized(&req) {
            tracing::warn!(peer = %peer, status = refusal.status.as_u16(), "purge refused");
            return refusal.into_response();
        }
        let body = match read_body(req.into_body(), MAX_PURGE_BODY).await {
            Ok(b) => b,
            Err(refusal) => return refusal.into_response(),
        };
        let targets = match purge::parse_request(&body) {
            Ok(t) => t,
            Err(e) => return problem(StatusCode::BAD_REQUEST, &e),
        };
        let removed = self.proxy.cache.purge(&targets).await;
        let m = &self.proxy.metrics;
        m.purges_local.fetch_add(1, Ordering::Relaxed);
        m.purged_objects
            .fetch_add(removed as u64, Ordering::Relaxed);
        tracing::info!(peer = %peer, targets = targets.len(), removed, "purge");
        json(
            StatusCode::OK,
            &serde_json::json!({ "removed": removed, "targets": targets.len() }),
        )
    }
}

impl Handler for Admin {
    async fn call(self: Arc<Self>, req: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
        match (req.method(), req.uri().path()) {
            (&Method::GET, "/healthz") => text(StatusCode::OK, "ok\n"),
            (&Method::GET, "/metrics") => {
                let p = &self.proxy;
                let body = p.metrics.render(&p.cache, &p.pool, p.in_flight());
                let mut resp = Response::new(full(body));
                resp.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; version=0.0.4"),
                );
                resp
            }
            (&Method::POST, "/purge") => self.purge(req, peer).await,
            (_, "/healthz" | "/metrics" | "/purge") => {
                problem(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
            }
            _ => problem(StatusCode::NOT_FOUND, "not found"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_comparison() {
        let expected: [u8; 32] = Sha256::digest(b"token").into();
        assert!(same_secret(&expected, b"token"));
        assert!(!same_secret(&expected, b"token "));
        assert!(!same_secret(&expected, b""));
    }
}
