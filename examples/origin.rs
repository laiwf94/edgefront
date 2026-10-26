//! A small origin server for the demo and the load test.
//!
//! Routes:
//!   /static/<name>?size=N  cacheable for five minutes, tagged "static"
//!   /slow                  cacheable, takes 300 ms (shows request collapsing)
//!   /versioned             max-age=2 with an entity tag; answers 304 when it matches
//!   /news                  max-age=2, stale-if-error=600, tagged "news"
//!   /stats                 JSON count of requests per path
//!   /healthz               200
//!   POST /bump             changes the /versioned and /news content
//!
//! Usage: cargo run --release --example origin -- 127.0.0.1:9100

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

#[derive(Default)]
struct State {
    version: AtomicU64,
    counts: Mutex<BTreeMap<String, u64>>,
}

fn reply(
    status: StatusCode,
    headers: &[(&str, String)],
    body: impl Into<Bytes>,
) -> Response<Full<Bytes>> {
    let mut b = Response::builder().status(status);
    for (k, v) in headers {
        b = b.header(*k, v);
    }
    b.body(Full::new(body.into())).expect("valid response")
}

async fn answer(state: Arc<State>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let path = req.uri().path().to_string();
    if path != "/stats" && path != "/healthz" {
        *state
            .counts
            .lock()
            .unwrap()
            .entry(path.clone())
            .or_default() += 1;
    }
    let version = state.version.load(Ordering::Relaxed);
    let etag = format!("\"v{version}\"");
    match (req.method(), path.as_str()) {
        (&Method::GET, "/healthz") => reply(StatusCode::OK, &[], "ok\n"),
        (&Method::GET, "/stats") => {
            let counts = state.counts.lock().unwrap().clone();
            let body = serde_json::to_string(&counts).expect("counts serialise");
            reply(
                StatusCode::OK,
                &[("content-type", "application/json".into())],
                body,
            )
        }
        (&Method::POST, "/bump") => {
            state.version.fetch_add(1, Ordering::Relaxed);
            reply(StatusCode::NO_CONTENT, &[], "")
        }
        (&Method::GET, "/slow") => {
            tokio::time::sleep(Duration::from_millis(300)).await;
            reply(
                StatusCode::OK,
                &[("cache-control", "max-age=60".into())],
                "slow but shared\n",
            )
        }
        (&Method::GET, "/versioned") => {
            let matches = req
                .headers()
                .get("if-none-match")
                .is_some_and(|v| v.as_bytes() == etag.as_bytes());
            let headers = [
                ("cache-control", "max-age=2".to_string()),
                ("etag", etag.clone()),
            ];
            if matches {
                reply(StatusCode::NOT_MODIFIED, &headers, "")
            } else {
                reply(StatusCode::OK, &headers, format!("version {version}\n"))
            }
        }
        (&Method::GET, "/news") => {
            let headers = [
                ("cache-control", "max-age=2, stale-if-error=600".to_string()),
                ("surrogate-key", "news".to_string()),
            ];
            reply(
                StatusCode::OK,
                &headers,
                format!("headlines, edition {version}\n"),
            )
        }
        (&Method::GET, p) if p.starts_with("/static/") => {
            let size = req
                .uri()
                .query()
                .and_then(|q| q.strip_prefix("size="))
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(4096)
                .min(16 << 20);
            let headers = [
                ("cache-control", "public, max-age=300".to_string()),
                ("surrogate-key", "static".to_string()),
                ("content-type", "application/octet-stream".to_string()),
            ];
            reply(StatusCode::OK, &headers, vec![b's'; size])
        }
        _ => reply(StatusCode::NOT_FOUND, &[], "not found\n"),
    }
}

#[tokio::main]
async fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9100".into());
    let listener = TcpListener::bind(&addr).await.expect("bind");
    println!(
        "origin listening on {}",
        listener.local_addr().expect("bound")
    );
    let state = Arc::new(State::default());
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let state = state.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(answer(state, req).await) }
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), svc)
                .await;
        });
    }
}
