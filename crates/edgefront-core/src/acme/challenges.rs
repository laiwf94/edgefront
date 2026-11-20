//! Proving control of a name to an ACME CA (RFC 8555 section 8).
//!
//! A [`Solver`] sets up one challenge type for a name and takes it down
//! again once the CA has decided. [`Http01`] is the first: the CA fetches
//! `http://<name>/.well-known/acme-challenge/<token>` and expects the key
//! authorization. [`Http01`] is a [`Handler`](crate::listener::Handler)
//! for a plain listener; it answers only tokens it holds, with
//! `text/plain`, and 404 for everything else.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http::{Request, Response, StatusCode, header};
use http_body_util::Full;
use hyper::body::Incoming;

use super::client::AcmeError;
use crate::listener::Handler;

/// A boxed future, so solvers can be trait objects.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Sets up and takes down one challenge type.
pub trait Solver: Send + Sync {
    /// The challenge type, as ACME names it: `http-01`, `tls-alpn-01` or
    /// `dns-01`.
    fn kind(&self) -> &'static str;
    /// Makes `domain`'s challenge answer with `key_auth`.
    fn prepare<'a>(
        &'a self,
        domain: &'a str,
        token: &'a str,
        key_auth: &'a str,
    ) -> BoxFuture<'a, Result<(), AcmeError>>;
    /// Removes what [`prepare`](Solver::prepare) set up.
    fn cleanup<'a>(
        &'a self,
        domain: &'a str,
        token: &'a str,
        key_auth: &'a str,
    ) -> BoxFuture<'a, ()>;
}

/// The path prefix HTTP-01 answers under.
pub const HTTP01_PREFIX: &str = "/.well-known/acme-challenge/";

/// The HTTP-01 responder: key authorizations by token.
#[derive(Debug, Default)]
pub struct Http01 {
    tokens: Mutex<HashMap<String, String>>,
}

impl Http01 {
    pub fn new() -> Arc<Http01> {
        Arc::new(Http01::default())
    }

    /// The answer for a request path, if it is a token held now.
    pub fn answer(&self, path: &str) -> Option<String> {
        let token = path.strip_prefix(HTTP01_PREFIX)?;
        self.tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(token)
            .cloned()
    }
}

impl Solver for Arc<Http01> {
    fn kind(&self) -> &'static str {
        "http-01"
    }

    fn prepare<'a>(
        &'a self,
        _: &'a str,
        token: &'a str,
        key_auth: &'a str,
    ) -> BoxFuture<'a, Result<(), AcmeError>> {
        Box::pin(async move {
            self.tokens
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(token.to_owned(), key_auth.to_owned());
            Ok(())
        })
    }

    fn cleanup<'a>(&'a self, _: &'a str, token: &'a str, _: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.tokens
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(token);
        })
    }
}

impl Handler for Http01 {
    type Body = Full<Bytes>;

    async fn call(self: Arc<Self>, req: Request<Incoming>, _: SocketAddr) -> Response<Full<Bytes>> {
        let found = (req.method() == http::Method::GET || req.method() == http::Method::HEAD)
            .then(|| self.answer(req.uri().path()))
            .flatten();
        match found {
            Some(key_auth) => {
                let mut resp = Response::new(Full::new(Bytes::from(key_auth)));
                resp.headers_mut().insert(
                    header::CONTENT_TYPE,
                    header::HeaderValue::from_static("text/plain"),
                );
                resp
            }
            None => {
                let mut resp = Response::new(Full::new(Bytes::from_static(b"not found\n")));
                *resp.status_mut() = StatusCode::NOT_FOUND;
                resp
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http01_answers_only_held_tokens() {
        let h = Http01::new();
        assert_eq!(h.kind(), "http-01");
        h.prepare("edge.test", "tok", "tok.thumb").await.unwrap();
        assert_eq!(
            h.answer("/.well-known/acme-challenge/tok").as_deref(),
            Some("tok.thumb")
        );
        assert_eq!(h.answer("/.well-known/acme-challenge/other"), None);
        assert_eq!(h.answer("/tok"), None);
        h.cleanup("edge.test", "tok", "tok.thumb").await;
        assert_eq!(h.answer("/.well-known/acme-challenge/tok"), None);
    }
}
