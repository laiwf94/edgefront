//! Test origins: small HTTP servers whose behaviour each test scripts.
#![allow(dead_code)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::request::Parts;
use http::{HeaderMap, Method, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::task::{JoinHandle, JoinSet};

pub type Handler = Arc<dyn Fn(&Parts, &Bytes) -> Response<Full<Bytes>> + Send + Sync>;

/// What the origin saw.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Bytes,
}

pub struct TestOrigin {
    pub addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Seen>>>,
    delay: Arc<Mutex<Duration>>,
    handler: Arc<Mutex<Handler>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

pub fn handler(
    f: impl Fn(&Parts, &Bytes) -> Response<Full<Bytes>> + Send + Sync + 'static,
) -> Handler {
    Arc::new(f)
}

/// A response with a body and header pairs.
pub fn reply(
    status: u16,
    headers: &[(&str, &str)],
    body: impl Into<Bytes>,
) -> Response<Full<Bytes>> {
    let mut b = Response::builder().status(StatusCode::from_u16(status).unwrap());
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Full::new(body.into())).unwrap()
}

impl TestOrigin {
    pub async fn start(h: Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let origin = TestOrigin {
            addr,
            hits: Arc::new(AtomicUsize::new(0)),
            seen: Arc::new(Mutex::new(Vec::new())),
            delay: Arc::new(Mutex::new(Duration::ZERO)),
            handler: Arc::new(Mutex::new(h)),
            task: Mutex::new(None),
        };
        origin.serve(listener);
        origin
    }

    fn serve(&self, listener: TcpListener) {
        let (hits, seen, delay, handler) = (
            self.hits.clone(),
            self.seen.clone(),
            self.delay.clone(),
            self.handler.clone(),
        );
        let task = tokio::spawn(async move {
            let mut conns = JoinSet::new();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let (hits, seen, delay, handler) =
                    (hits.clone(), seen.clone(), delay.clone(), handler.clone());
                conns.spawn(async move {
                    let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                        let (hits, seen, delay, handler) =
                            (hits.clone(), seen.clone(), delay.clone(), handler.clone());
                        async move {
                            let (parts, body) = req.into_parts();
                            let body = body
                                .collect()
                                .await
                                .map(|c| c.to_bytes())
                                .unwrap_or_default();
                            // Health probes are not counted as hits.
                            if parts.uri.path() != "/healthz" {
                                hits.fetch_add(1, Ordering::SeqCst);
                                seen.lock().unwrap().push(Seen {
                                    method: parts.method.clone(),
                                    uri: parts.uri.clone(),
                                    headers: parts.headers.clone(),
                                    body: body.clone(),
                                });
                            }
                            let d = *delay.lock().unwrap();
                            if !d.is_zero() && parts.uri.path() != "/healthz" {
                                tokio::time::sleep(d).await;
                            }
                            let h = handler.lock().unwrap().clone();
                            Ok::<_, Infallible>(h(&parts, &body))
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        *self.task.lock().unwrap() = Some(task);
    }

    /// Stops accepting and drops every open connection.
    pub fn stop(&self) {
        if let Some(t) = self.task.lock().unwrap().take() {
            t.abort();
        }
    }

    /// Starts serving again on the same address.
    pub async fn restart(&self) {
        self.stop();
        let mut tries = 0;
        let listener = loop {
            match TcpListener::bind(self.addr).await {
                Ok(l) => break l,
                Err(e) if tries < 50 => {
                    tries += 1;
                    let _ = e;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("rebind {}: {e}", self.addr),
            }
        };
        self.serve(listener);
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub fn last(&self) -> Seen {
        self.seen().pop().expect("origin saw no request")
    }

    pub fn set_delay(&self, d: Duration) {
        *self.delay.lock().unwrap() = d;
    }

    pub fn set_handler(&self, h: Handler) {
        *self.handler.lock().unwrap() = h;
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for TestOrigin {
    fn drop(&mut self) {
        self.stop();
    }
}

/// An address nothing listens on.
pub async fn closed_port() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap()
}

/// A server that accepts connections and never answers.
pub async fn black_hole() -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });
    (addr, task)
}

/// A server that answers every connection with bytes that are not HTTP.
pub async fn garbage_server() -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let _ = s.write_all(b"this is not http\r\n\r\n").await;
            let _ = s.shutdown().await;
        }
    });
    (addr, task)
}

/// Polls `f` until it is true or a few seconds pass.
pub async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..300 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}
