//! The admin listener: health, metrics and purges.
//!
//! It binds to localhost by default. `POST /purge` needs the admin token as
//! a bearer token; without a configured token the purge API is off. The
//! routing, token check and JSON errors come from `edgefront_core::admin`;
//! this module holds cachet's routes.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use edgefront_core::admin::{BearerToken, Router, json, problem};
use edgefront_core::metrics;
use http::header::{self, HeaderValue};
use http::{Method, Request, Response, StatusCode};
use hyper::body::Incoming;

pub use edgefront_core::admin::{Refusal, read_body};

use crate::cluster::{Cluster, PEER_PATH};
use crate::proxy::Proxy;
use crate::purge::{self, MAX_PURGE_BODY};
use crate::respond::{Body, full};

/// What cachet's admin routes share.
pub struct Admin {
    proxy: Arc<Proxy>,
    token: Option<BearerToken>,
    cluster: Option<Arc<Cluster>>,
}

impl Admin {
    pub fn new(proxy: Arc<Proxy>, token: Option<&str>, cluster: Option<Arc<Cluster>>) -> Self {
        Admin {
            proxy,
            token: token.map(BearerToken::new),
            cluster,
        }
    }

    /// The admin API: `GET /healthz`, `GET /metrics`, `POST /purge` and the
    /// peer purge endpoint.
    pub fn router(self) -> Router<Admin> {
        Router::new(Arc::new(self))
            .health()
            .route(
                Method::GET,
                "/metrics",
                |admin: Arc<Admin>, _, _| async move { admin.metrics() },
            )
            .route(
                Method::POST,
                "/purge",
                |admin: Arc<Admin>, req, peer| async move { admin.purge(req, peer).await },
            )
            .any(PEER_PATH, |admin: Arc<Admin>, req, peer| async move {
                match &admin.cluster {
                    Some(cluster) => cluster.receive(req, peer).await,
                    None => problem(StatusCode::NOT_FOUND, "this node is not in a cluster"),
                }
            })
    }

    fn metrics(&self) -> Response<Body> {
        let p = &self.proxy;
        let body = p.metrics.render(&p.cache, &p.pool, p.in_flight());
        let mut resp = Response::new(full(body));
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(metrics::CONTENT_TYPE),
        );
        resp
    }

    fn authorized(&self, req: &Request<Incoming>) -> Result<(), Refusal> {
        let Some(expected) = &self.token else {
            return Err(Refusal {
                status: StatusCode::FORBIDDEN,
                message: "purge API is disabled: set CACHET_ADMIN_TOKEN",
            });
        };
        expected.check(req.headers())
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
        let (removed, id) = match &self.cluster {
            Some(cluster) => {
                let (removed, id) = cluster.purge_local(&targets).await;
                (removed, Some(id))
            }
            None => (self.proxy.cache.purge(&targets).await, None),
        };
        let m = &self.proxy.metrics;
        m.purges_local.fetch_add(1, Ordering::Relaxed);
        m.purged_objects
            .fetch_add(removed as u64, Ordering::Relaxed);
        tracing::info!(peer = %peer, id = id.as_deref().unwrap_or(""), targets = targets.len(), removed, "purge");
        let mut body = serde_json::json!({ "removed": removed, "targets": targets.len() });
        if let Some(id) = id {
            body["id"] = id.into();
        }
        json(StatusCode::OK, &body)
    }
}
