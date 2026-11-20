//! A fake Kubernetes API server for the discovery tests: just enough of
//! `discovery.k8s.io/v1` EndpointSlices for the Kubernetes source. It lists
//! and watches the slices of one namespace (a label selector of
//! `kubernetes.io/service-name=<name>` filters them), numbers every change
//! with a resource version, streams changes to open watches as JSON
//! lines, answers a watch from a version older than its log with 410
//! Gone, and can drop every open watch or fail every request with 500.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::{Body, Frame, Incoming};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

/// One endpoint of a slice: its address and readiness (`None` leaves the
/// condition unset).
pub type Pod<'a> = (&'a str, Option<bool>);

struct State {
    version: u64,
    /// name -> (service, slice JSON)
    slices: BTreeMap<String, (String, Value)>,
    /// (version, event type, slice JSON, service)
    log: Vec<(u64, &'static str, Value, String)>,
    /// The oldest version a watch may resume from.
    floor: u64,
}

pub struct FakeKube {
    pub addr: SocketAddr,
    state: Arc<Mutex<State>>,
    changed: watch::Sender<u64>,
    /// Bumped to make every open watch end.
    cut: watch::Sender<u64>,
    failing: Arc<AtomicBool>,
    pub lists: Arc<AtomicU64>,
    pub watches: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeKube {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn slice_json(name: &str, service: &str, version: u64, port: u16, pods: &[Pod<'_>]) -> Value {
    let endpoints: Vec<Value> = pods
        .iter()
        .map(|(a, ready)| {
            let mut conditions = serde_json::Map::new();
            if let Some(r) = ready {
                conditions.insert("ready".into(), json!(r));
            }
            json!({"addresses": [a], "conditions": conditions, "zone": "zone-a"})
        })
        .collect();
    json!({
        "apiVersion": "discovery.k8s.io/v1",
        "kind": "EndpointSlice",
        "metadata": {
            "name": name,
            "namespace": "prod",
            "resourceVersion": version.to_string(),
            "labels": {"kubernetes.io/service-name": service},
        },
        "addressType": "IPv4",
        "ports": [{"name": "http", "port": port, "protocol": "TCP"}],
        "endpoints": endpoints,
    })
}

impl FakeKube {
    pub async fn start() -> FakeKube {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(State {
            version: 100,
            slices: BTreeMap::new(),
            log: Vec::new(),
            floor: 100,
        }));
        let (changed, _) = watch::channel(100);
        let (cut, _) = watch::channel(0);
        let failing = Arc::new(AtomicBool::new(false));
        let lists = Arc::new(AtomicU64::new(0));
        let watches = Arc::new(AtomicU64::new(0));
        let ctx = Ctx {
            state: Arc::clone(&state),
            changed: changed.clone(),
            cut: cut.clone(),
            failing: Arc::clone(&failing),
            lists: Arc::clone(&lists),
            watches: Arc::clone(&watches),
        };
        let task = tokio::spawn(async move {
            loop {
                let Ok((conn, _)) = listener.accept().await else {
                    return;
                };
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |req| {
                        let ctx = ctx.clone();
                        async move { Ok::<_, Infallible>(ctx.handle(req)) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(conn), service)
                        .await;
                });
            }
        });
        FakeKube {
            addr,
            state,
            changed,
            cut,
            failing,
            lists,
            watches,
            task,
        }
    }

    /// The API server's URL.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// How a source reaches this server.
    pub fn api(&self) -> edgefront_proxy::discovery::k8s::ApiAccess {
        edgefront_proxy::discovery::k8s::ApiAccess::new(&self.url())
    }

    /// Creates or replaces slice `name` of `service` with `pods` on `port`.
    pub fn apply(&self, name: &str, service: &str, port: u16, pods: &[Pod<'_>]) {
        let mut s = self.state.lock().unwrap();
        s.version += 1;
        let v = s.version;
        let kind = if s.slices.contains_key(name) {
            "MODIFIED"
        } else {
            "ADDED"
        };
        let obj = slice_json(name, service, v, port, pods);
        s.slices
            .insert(name.to_owned(), (service.to_owned(), obj.clone()));
        s.log.push((v, kind, obj, service.to_owned()));
        drop(s);
        self.changed.send_replace(v);
    }

    /// Deletes slice `name`.
    pub fn delete(&self, name: &str) {
        let mut s = self.state.lock().unwrap();
        let Some((service, mut obj)) = s.slices.remove(name) else {
            return;
        };
        s.version += 1;
        let v = s.version;
        obj["metadata"]["resourceVersion"] = json!(v.to_string());
        s.log.push((v, "DELETED", obj, service));
        drop(s);
        self.changed.send_replace(v);
    }

    /// Waits until `n` watches have been opened in all.
    pub async fn watched(&self, n: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.watches.load(Ordering::SeqCst) < n {
            assert!(std::time::Instant::now() < deadline, "watch {n} never came");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// Ends every open watch, as an API server restart or a load balancer
    /// timeout does.
    pub fn cut_watches(&self) {
        self.cut.send_modify(|n| *n += 1);
    }

    /// Forgets the change log, so a watch resuming from before now is
    /// answered with 410 Gone and the client must list again.
    pub fn compact(&self) {
        let mut s = self.state.lock().unwrap();
        s.floor = s.version;
        s.log.clear();
    }

    /// Answers every request with 500 (and ends open watches) or stops.
    pub fn fail(&self, on: bool) {
        self.failing.store(on, Ordering::SeqCst);
        if on {
            self.cut_watches();
        }
    }
}

#[derive(Clone)]
struct Ctx {
    state: Arc<Mutex<State>>,
    changed: watch::Sender<u64>,
    cut: watch::Sender<u64>,
    failing: Arc<AtomicBool>,
    lists: Arc<AtomicU64>,
    watches: Arc<AtomicU64>,
}

type Reply = Response<Either>;

fn query(req: &Request<Incoming>) -> BTreeMap<String, String> {
    req.uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_owned(), decode(v)))
        .collect()
}

fn decode(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

fn json_reply(status: u16, body: &Value) -> Reply {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Either::Full(Full::new(Bytes::from(body.to_string()))))
        .unwrap()
}

impl Ctx {
    fn handle(&self, req: Request<Incoming>) -> Reply {
        if self.failing.load(Ordering::SeqCst) {
            return json_reply(
                500,
                &json!({"kind":"Status","apiVersion":"v1","status":"Failure","message":"etcd is down","code":500}),
            );
        }
        let path = req.uri().path();
        if path != "/apis/discovery.k8s.io/v1/namespaces/prod/endpointslices" {
            return json_reply(
                404,
                &json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}),
            );
        }
        let q = query(&req);
        let service = q
            .get("labelSelector")
            .and_then(|l| l.strip_prefix("kubernetes.io/service-name="))
            .unwrap_or("")
            .to_owned();
        if q.get("watch").is_some_and(|w| w == "true" || w == "1") {
            self.watches.fetch_add(1, Ordering::SeqCst);
            let from: u64 = q
                .get("resourceVersion")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            return self.watch(service, from);
        }
        self.lists.fetch_add(1, Ordering::SeqCst);
        let s = self.state.lock().unwrap();
        let items: Vec<Value> = s
            .slices
            .values()
            .filter(|(svc, _)| *svc == service)
            .map(|(_, v)| v.clone())
            .collect();
        json_reply(
            200,
            &json!({
                "apiVersion": "discovery.k8s.io/v1",
                "kind": "EndpointSliceList",
                "metadata": {"resourceVersion": s.version.to_string()},
                "items": items,
            }),
        )
    }

    fn watch(&self, service: String, from: u64) -> Reply {
        let (tx, rx) = mpsc::channel::<Bytes>(64);
        {
            let s = self.state.lock().unwrap();
            if from < s.floor {
                let gone = json!({"type": "ERROR", "object": {
                    "kind":"Status","apiVersion":"v1","status":"Failure",
                    "reason":"Expired","code":410,"message":"too old resource version"}});
                let _ = tx.try_send(Bytes::from(format!("{gone}\n")));
                return stream_reply(rx);
            }
        }
        let state = Arc::clone(&self.state);
        let mut changed = self.changed.subscribe();
        let mut cut = self.cut.subscribe();
        cut.borrow_and_update();
        tokio::spawn(async move {
            let mut sent = from;
            loop {
                let lines: Vec<(u64, String)> = {
                    let s = state.lock().unwrap();
                    s.log
                        .iter()
                        .filter(|(v, _, _, svc)| *v > sent && *svc == service)
                        .map(|(v, kind, obj, _)| {
                            (*v, json!({"type": kind, "object": obj}).to_string())
                        })
                        .collect()
                };
                for (v, line) in lines {
                    if tx.send(Bytes::from(line + "\n")).await.is_err() {
                        return;
                    }
                    sent = v;
                }
                tokio::select! {
                    r = changed.changed() => if r.is_err() { return },
                    _ = cut.changed() => return,
                    () = tx.closed() => return,
                }
            }
        });
        stream_reply(rx)
    }
}

fn stream_reply(rx: mpsc::Receiver<Bytes>) -> Reply {
    Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(Either::Stream(rx))
        .unwrap()
}

/// A whole body or one streamed from a channel.
pub enum Either {
    Full(Full<Bytes>),
    Stream(mpsc::Receiver<Bytes>),
}

impl Body for Either {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match self.get_mut() {
            Either::Full(f) => Pin::new(f).poll_frame(cx),
            Either::Stream(rx) => rx.poll_recv(cx).map(|b| b.map(|b| Ok(Frame::data(b)))),
        }
    }
}
