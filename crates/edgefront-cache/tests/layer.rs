//! CacheLayer through the filter phases, against a scripted upstream.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use edgefront_cache::config::CacheConfig;
use edgefront_cache::headers::http_date;
use edgefront_cache::layer::{
    CACHE_STATUS, CacheLayer, Clock, LayerSettings, Observer, Outcome, Prefixed, SystemClock,
    is_error_status,
};
use edgefront_cache::purge::PurgeTarget;
use edgefront_cache::store::{Cache, CacheSettings};
use edgefront_core::body::{Body, full, text};
use edgefront_core::config::Route;
use edgefront_core::filter::{self, Decision, Delivery, Filter, RequestHead};
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper::body::{Body as HttpBody, Frame};
use tokio::sync::Notify;

struct TestClock(Mutex<SystemTime>);

impl TestClock {
    fn new() -> Arc<Self> {
        Arc::new(TestClock(Mutex::new(
            UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        )))
    }

    fn advance(&self, secs: u64) {
        *self.0.lock().unwrap() += Duration::from_secs(secs);
    }
}

impl Clock for TestClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap()
    }
}

/// What the scripted upstream does for one request.
enum Script {
    Respond(Response<Body>),
    /// A `502` the proxy wrote.
    Fail(&'static str),
    /// A `400` the proxy wrote, about the client's request.
    Refuse,
    /// The script item once the gate opens.
    Wait(Arc<Notify>, Box<Script>),
}

/// An upstream that answers from a queue and records what it was sent.
#[derive(Default)]
struct Upstream {
    script: Mutex<VecDeque<Script>>,
    seen: Mutex<Vec<(Method, String, HeaderMap)>>,
    calls: AtomicUsize,
}

impl Upstream {
    fn push(&self, resp: Response<Body>) {
        self.script.lock().unwrap().push_back(Script::Respond(resp));
    }

    fn fail(&self, reason: &'static str) {
        self.script.lock().unwrap().push_back(Script::Fail(reason));
    }

    /// Queues `script` behind a gate, and returns the gate.
    fn gated(&self, script: Script) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        self.script
            .lock()
            .unwrap()
            .push_back(Script::Wait(gate.clone(), Box::new(script)));
        gate
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn last_headers(&self) -> HeaderMap {
        self.seen.lock().unwrap().last().unwrap().2.clone()
    }

    async fn send(&self, req: Request<()>) -> (Response<Body>, Delivery) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.seen.lock().unwrap().push((
            req.method().clone(),
            req.uri().to_string(),
            req.headers().clone(),
        ));
        let mut next = self.script.lock().unwrap().pop_front();
        loop {
            return match next {
                Some(Script::Respond(r)) => (r, Delivery::Upstream),
                Some(Script::Fail(reason)) => (
                    text(StatusCode::BAD_GATEWAY, "proxy error\n"),
                    Delivery::Generated(reason),
                ),
                Some(Script::Refuse) => (
                    text(StatusCode::BAD_REQUEST, "bad request\n"),
                    Delivery::Generated("bad_request"),
                ),
                Some(Script::Wait(gate, then)) => {
                    gate.notified().await;
                    next = Some(*then);
                    continue;
                }
                None => panic!("unexpected upstream request"),
            };
        }
    }
}

fn route(host: &str, prefix: &str) -> Route {
    Route {
        host: host.into(),
        path_prefix: prefix.into(),
        cluster: "c".into(),
    }
}

struct Harness {
    layer: Arc<CacheLayer>,
    upstream: Arc<Upstream>,
    clock: Arc<TestClock>,
}

/// Route 0 (`a`, `/`) is cached with the `[cache]` text given, route 1
/// (`b`, `/`) is not.
fn harness_with(cache_toml: &str, body_timeout: Duration) -> Harness {
    let cfg: CacheConfig = toml::from_str(cache_toml).unwrap();
    let policies = cfg.resolve(&[route("a", "/"), route("b", "/")]).unwrap();
    let cache = Arc::new(
        Cache::new(&CacheSettings {
            memory_bytes: 1 << 20,
            shards: 1,
            max_object_bytes: cfg.max_object_bytes as usize,
            max_variants: cfg.max_variants,
            ..CacheSettings::default()
        })
        .unwrap(),
    );
    let clock = TestClock::new();
    let layer = CacheLayer::new(
        cache,
        policies,
        LayerSettings {
            name: "edge".into(),
            body_timeout,
            via: Some(HeaderValue::from_static("1.1 edge")),
        },
        clock.clone(),
    );
    Harness {
        layer,
        upstream: Arc::default(),
        clock,
    }
}

const ROUTE: &str = "routes = [{ host = \"a\", path_prefix = \"/\" }]\n";

fn harness() -> Harness {
    harness_with(ROUTE, Duration::from_secs(5))
}

fn origin(status: u16, headers: &[(&str, &str)], body: &'static str) -> Response<Body> {
    let mut resp = Response::new(full(body));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap();
    for (k, v) in headers {
        resp.headers_mut().append(
            http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    resp
}

impl Harness {
    fn date(&self) -> String {
        http_date(self.clock.now()).to_str().unwrap().to_owned()
    }

    async fn send(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> Response<Body> {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "a");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let filters = self.layer.route_filters();
        let upstream = self.upstream.clone();
        filter::run(filters.get(0), 0, req.body(()).unwrap(), move |r| {
            let upstream = upstream.clone();
            async move { upstream.send(r).await }
        })
        .await
    }

    async fn get(&self, path: &str) -> Response<Body> {
        self.send("GET", path, &[]).await
    }
}

fn status(resp: &Response<Body>) -> &str {
    resp.headers()[CACHE_STATUS].to_str().unwrap()
}

async fn body(resp: Response<Body>) -> Bytes {
    resp.into_body().collect().await.unwrap().to_bytes()
}

#[tokio::test]
async fn a_miss_is_stored_and_the_next_request_is_a_hit() {
    let h = harness();
    h.upstream.push(origin(
        200,
        &[
            ("cache-control", "max-age=60"),
            ("content-length", "5"),
            ("surrogate-key", "t1"),
        ],
        "hello",
    ));
    let resp = h.get("/x").await;
    assert_eq!(
        status(&resp),
        "edge; fwd=miss; fwd-status=200; stored; ttl=60"
    );
    assert!(!resp.headers().contains_key("surrogate-key"));
    assert_eq!(body(resp).await, "hello");
    h.clock.advance(10);
    let resp = h.get("/x").await;
    assert_eq!(status(&resp), "edge; hit; ttl=50");
    assert_eq!(resp.headers()["age"], "10");
    assert_eq!(body(resp).await, "hello");
    assert_eq!(h.upstream.calls(), 1);
    // HEAD is answered from the copy too.
    let resp = h.send("HEAD", "/x", &[]).await;
    assert_eq!(status(&resp), "edge; hit; ttl=50");
    assert_eq!(body(resp).await, "");
    assert_eq!(h.upstream.calls(), 1);
    // A tag purge removes it.
    let removed = h
        .layer
        .cache()
        .purge(&[PurgeTarget::Tag("t1".into())])
        .await;
    assert_eq!(removed, 1);
    h.upstream
        .push(origin(200, &[("cache-control", "max-age=60")], "again"));
    assert_eq!(body(h.get("/x").await).await, "again");
}

#[tokio::test]
async fn routes_without_a_policy_have_no_filter() {
    let h = harness();
    let filters = h.layer.route_filters();
    assert!(filters.get(0).is_some());
    assert!(filters.get(1).is_none());
    assert_eq!(filters.filtered(), 1);
    assert_eq!(h.layer.name(), "edge");
    // Called on such a route anyway, the layer forwards untouched.
    let (method, uri) = (Method::GET, "/x".parse().unwrap());
    let mut headers = HeaderMap::new();
    headers.insert("host", HeaderValue::from_static("b"));
    headers.insert("if-none-match", HeaderValue::from_static("\"v\""));
    let d = h
        .layer
        .request(RequestHead {
            route: 1,
            method: &method,
            uri: &uri,
            headers: &mut headers,
        })
        .await;
    assert!(matches!(d, Decision::Forward(None)));
    assert!(headers.contains_key("if-none-match"));
    // And so does a request without a host.
    let mut headers = HeaderMap::new();
    let d = h
        .layer
        .request(RequestHead {
            route: 0,
            method: &method,
            uri: &uri,
            headers: &mut headers,
        })
        .await;
    assert!(matches!(d, Decision::Forward(None)));
}

#[tokio::test]
async fn a_stale_copy_is_revalidated_with_its_validators() {
    let h = harness();
    let date = h.date();
    h.upstream.push(origin(
        200,
        &[
            ("cache-control", "max-age=10"),
            ("etag", "\"v1\""),
            ("last-modified", &date),
        ],
        "body",
    ));
    h.get("/r").await;
    h.clock.advance(20);
    h.upstream.push(origin(
        304,
        &[("cache-control", "max-age=30"), ("etag", "\"v1\"")],
        "",
    ));
    // The client's own conditionals and range are not sent upstream.
    let resp = h
        .send(
            "GET",
            "/r",
            &[("if-none-match", "\"other\""), ("range", "bytes=0-1")],
        )
        .await;
    let sent = h.upstream.last_headers();
    assert_eq!(sent["if-none-match"], "\"v1\"");
    // The entity tag is preferred; Last-Modified is not sent with it.
    assert!(!sent.contains_key("if-modified-since"));
    assert!(!sent.contains_key("range"));
    assert_eq!(
        status(&resp),
        "edge; fwd=stale; fwd-status=304; stored; ttl=30"
    );
    // The client's range applies to the refreshed copy.
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body(resp).await, "bo");
    let resp = h.send("GET", "/r", &[("if-none-match", "\"v1\"")]).await;
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(status(&resp), "edge; hit; ttl=30");
    assert_eq!(h.upstream.calls(), 2);
}

#[tokio::test]
async fn without_an_entity_tag_last_modified_is_the_validator() {
    let h = harness();
    let date = h.date();
    h.upstream.push(origin(
        200,
        &[("cache-control", "max-age=10"), ("last-modified", &date)],
        "lm",
    ));
    h.get("/lm").await;
    h.clock.advance(20);
    h.upstream
        .push(origin(304, &[("cache-control", "max-age=10")], ""));
    let resp = h.get("/lm").await;
    assert_eq!(
        h.upstream.last_headers()["if-modified-since"],
        date.as_str()
    );
    assert_eq!(body(resp).await, "lm");
}

#[tokio::test]
async fn a_304_for_another_representation_is_fetched_again_in_full() {
    let h = harness();
    h.upstream.push(origin(
        200,
        &[("cache-control", "max-age=1"), ("etag", "\"v1\"")],
        "one",
    ));
    h.get("/m").await;
    h.clock.advance(5);
    h.upstream.push(origin(304, &[("etag", "\"v2\"")], ""));
    h.upstream.push(origin(
        200,
        &[("cache-control", "max-age=60"), ("etag", "\"v2\"")],
        "two",
    ));
    let resp = h.send("GET", "/m", &[("x-trace", "t")]).await;
    assert_eq!(
        status(&resp),
        "edge; fwd=stale; fwd-status=200; stored; ttl=60"
    );
    assert_eq!(resp.extensions().get::<Outcome>(), Some(&Outcome::Miss));
    assert_eq!(body(resp).await, "two");
    assert_eq!(h.upstream.calls(), 3);
    // The second request is the first without its validator.
    let sent = h.upstream.last_headers();
    assert!(!sent.contains_key("if-none-match"));
    assert_eq!(sent["x-trace"], "t");
    // A 304 to that one is not taken as a revalidation again.
    h.clock.advance(100);
    h.upstream.push(origin(304, &[("etag", "\"v3\"")], ""));
    h.upstream.push(origin(304, &[("etag", "\"v3\"")], ""));
    let resp = h.get("/m").await;
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(status(&resp), "edge; fwd=stale; fwd-status=304");
}

#[tokio::test]
async fn unstorable_responses_pass_and_remove_an_old_copy() {
    let h = harness();
    h.upstream.push(origin(
        200,
        &[("cache-control", "private"), ("surrogate-key", "k")],
        "mine",
    ));
    let resp = h.get("/p").await;
    assert_eq!(status(&resp), "edge; fwd=miss; fwd-status=200");
    assert!(!resp.headers().contains_key("surrogate-key"));
    assert_eq!(body(resp).await, "mine");
    // Stored, then turned private by the origin.
    h.upstream
        .push(origin(200, &[("cache-control", "max-age=1")], "public"));
    h.get("/q").await;
    h.clock.advance(5);
    h.upstream
        .push(origin(200, &[("cache-control", "no-store")], "private now"));
    let resp = h.get("/q").await;
    assert_eq!(status(&resp), "edge; fwd=stale; fwd-status=200");
    assert!(
        h.layer
            .cache()
            .lookup(&"a/q".into(), &HeaderMap::new())
            .await
            .is_none()
    );
}

#[tokio::test]
async fn bodies_over_the_route_limit_stream_through() {
    let h = harness_with(
        "routes = [{ host = \"a\", path_prefix = \"/\", max_object_bytes = 4 }]\n",
        Duration::from_secs(5),
    );
    // Declared too large.
    h.upstream.push(origin(
        200,
        &[("cache-control", "max-age=60"), ("content-length", "6")],
        "123456",
    ));
    let resp = h.get("/big").await;
    assert_eq!(status(&resp), "edge; fwd=miss; fwd-status=200");
    assert_eq!(body(resp).await, "123456");
    // Found too large while reading: what was read is sent first.
    let chunks = Chunks(VecDeque::from(["12", "34", "56", "78"]));
    let mut resp = Response::new(chunks.boxed());
    resp.headers_mut()
        .insert("cache-control", HeaderValue::from_static("max-age=60"));
    h.upstream.push(resp);
    let resp = h.get("/big2").await;
    assert_eq!(status(&resp), "edge; fwd=miss; fwd-status=200");
    assert_eq!(body(resp).await, "12345678");
    assert_eq!(h.layer.cache().memory().len(), 0);
    // A body within the limit is stored.
    h.upstream
        .push(origin(200, &[("cache-control", "max-age=60")], "1234"));
    let resp = h.get("/small").await;
    assert!(status(&resp).ends_with("stored; ttl=60"));
}

/// A body that yields its chunks one frame at a time.
struct Chunks(VecDeque<&'static str>);

impl HttpBody for Chunks {
    type Data = Bytes;
    type Error = edgefront_core::body::BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        Poll::Ready(self.0.pop_front().map(|c| Ok(Frame::data(Bytes::from(c)))))
    }
}

/// A body that never ends, or fails at once.
enum Broken {
    Stall,
    Reset,
}

impl HttpBody for Broken {
    type Data = Bytes;
    type Error = edgefront_core::body::BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        match *self {
            Broken::Stall => Poll::Pending,
            Broken::Reset => Poll::Ready(Some(Err("reset".into()))),
        }
    }
}

#[tokio::test]
async fn a_body_that_stalls_or_breaks_is_an_error_or_a_stale_copy() {
    let h = harness_with(ROUTE, Duration::from_millis(50));
    let mut resp = Response::new(Broken::Stall.boxed());
    resp.headers_mut()
        .insert("cache-control", HeaderValue::from_static("max-age=60"));
    h.upstream.push(resp);
    let resp = h.get("/s").await;
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(status(&resp), "edge; fwd=miss; detail=timeout");
    let mut resp = Response::new(Broken::Reset.boxed());
    resp.headers_mut()
        .insert("cache-control", HeaderValue::from_static("max-age=60"));
    h.upstream.push(resp);
    let resp = h.get("/s").await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(status(&resp), "edge; fwd=miss; detail=protocol");
    // With a copy that allows stale-if-error, the copy is served instead.
    h.upstream.push(origin(
        200,
        &[("cache-control", "max-age=1, stale-if-error=600")],
        "kept",
    ));
    h.get("/s").await;
    h.clock.advance(5);
    let mut resp = Response::new(Broken::Reset.boxed());
    resp.headers_mut()
        .insert("cache-control", HeaderValue::from_static("max-age=60"));
    h.upstream.push(resp);
    let resp = h.get("/s").await;
    assert_eq!(
        status(&resp),
        "edge; hit; ttl=-4; fwd=stale; detail-origin=protocol; detail=stale-if-error"
    );
    assert_eq!(body(resp).await, "kept");
}

#[tokio::test]
async fn stale_if_error_covers_upstream_errors_and_proxy_failures() {
    let h = harness();
    h.upstream.push(origin(
        200,
        &[("cache-control", "max-age=1, stale-if-error=60")],
        "old",
    ));
    h.get("/e").await;
    h.clock.advance(10);
    h.upstream.push(origin(503, &[], "down"));
    let resp = h.get("/e").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        status(&resp),
        "edge; hit; ttl=-9; fwd=stale; fwd-status=503; detail=stale-if-error"
    );
    assert_eq!(body(resp).await, "old");
    h.upstream.fail("connect");
    let resp = h.get("/e").await;
    assert_eq!(
        status(&resp),
        "edge; hit; ttl=-9; fwd=stale; detail-origin=connect; detail=stale-if-error"
    );
    // Past the window the error goes through.
    h.clock.advance(100);
    h.upstream.fail("timeout");
    let resp = h.get("/e").await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(status(&resp), "edge; fwd=stale; detail=timeout");
    h.upstream.push(origin(500, &[], "err"));
    let resp = h.get("/e").await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(status(&resp), "edge; fwd=stale; fwd-status=500");
    assert!(is_error_status(StatusCode::GATEWAY_TIMEOUT));
    assert!(!is_error_status(StatusCode::NOT_FOUND));
}

#[tokio::test]
async fn request_directives() {
    let h = harness();
    // only-if-cached with nothing stored.
    let resp = h
        .send("GET", "/o", &[("cache-control", "only-if-cached")])
        .await;
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(status(&resp), "edge; fwd=miss; detail=only-if-cached");
    assert_eq!(h.upstream.calls(), 0);
    // no-store passes, and keeps the client's conditionals.
    h.upstream
        .push(origin(200, &[("cache-control", "max-age=60")], "n"));
    let resp = h
        .send(
            "GET",
            "/o",
            &[("cache-control", "no-store"), ("if-none-match", "\"z\"")],
        )
        .await;
    assert_eq!(status(&resp), "edge; fwd=request; fwd-status=200");
    assert_eq!(h.upstream.last_headers()["if-none-match"], "\"z\"");
    assert_eq!(h.layer.cache().memory().len(), 0);
    // Stored, then a request that will not take a fresh copy.
    h.upstream
        .push(origin(200, &[("cache-control", "max-age=60")], "v1"));
    h.get("/o").await;
    h.upstream
        .push(origin(200, &[("cache-control", "max-age=60")], "v2"));
    let resp = h.send("GET", "/o", &[("cache-control", "no-cache")]).await;
    assert!(status(&resp).starts_with("edge; fwd=request; fwd-status=200"));
    assert_eq!(body(resp).await, "v2");
    // max-stale accepts a stale copy.
    h.clock.advance(100);
    let resp = h
        .send("GET", "/o", &[("cache-control", "max-stale=60")])
        .await;
    assert_eq!(status(&resp), "edge; hit; ttl=-40; detail=max-stale");
    // HEAD without a usable copy passes.
    h.upstream.push(origin(200, &[], ""));
    let resp = h.send("HEAD", "/o", &[]).await;
    assert_eq!(status(&resp), "edge; fwd=miss; fwd-status=200");
}

/// Waits, with a bound, until `done` holds.
async fn until(mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("condition reached in time");
}

/// Starts a GET on its own task.
fn spawn_get(
    h: &Arc<Harness>,
    path: &'static str,
    headers: &'static [(&'static str, &'static str)],
) -> tokio::task::JoinHandle<(String, StatusCode, Bytes)> {
    let h = h.clone();
    tokio::spawn(async move {
        let resp = h.send("GET", path, headers).await;
        let st = status(&resp).to_owned();
        let code = resp.status();
        (st, code, body(resp).await)
    })
}

/// Gives spawned requests time to reach the point where they wait.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(30)).await;
}

#[tokio::test]
async fn stale_while_revalidate_serves_stale_and_refreshes_in_the_background() {
    let h = harness();
    h.upstream.push(origin(
        200,
        &[
            ("cache-control", "max-age=1, stale-while-revalidate=60"),
            ("etag", "\"a\""),
        ],
        "a",
    ));
    h.get("/w").await;
    h.clock.advance(5);
    let gate = h.upstream.gated(Script::Respond(origin(
        304,
        &[("etag", "\"a\""), ("cache-control", "max-age=10")],
        "",
    )));
    let resp = h.get("/w").await;
    assert_eq!(
        status(&resp),
        "edge; hit; ttl=-4; detail=stale-while-revalidate"
    );
    assert_eq!(body(resp).await, "a");
    until(|| h.upstream.calls() == 2).await;
    assert_eq!(h.upstream.last_headers()["if-none-match"], "\"a\"");
    // While the refresh runs, others get the stale copy without a second one.
    let resp = h.get("/w").await;
    assert_eq!(
        status(&resp),
        "edge; hit; ttl=-4; detail=stale-while-revalidate"
    );
    assert_eq!(h.layer.in_flight(), 1);
    gate.notify_one();
    until(|| h.layer.in_flight() == 0).await;
    let resp = h.get("/w").await;
    assert_eq!(status(&resp), "edge; hit; ttl=10");
    assert_eq!(h.upstream.calls(), 2);
}

#[tokio::test]
async fn concurrent_misses_share_one_fill() {
    let h = Arc::new(harness());
    let gate = h.upstream.gated(Script::Respond(origin(
        200,
        &[("cache-control", "max-age=60")],
        "shared",
    )));
    let leader = spawn_get(&h, "/c", &[]);
    until(|| h.layer.in_flight() == 1).await;
    let followers: Vec<_> = (0..5).map(|_| spawn_get(&h, "/c", &[])).collect();
    settle().await;
    gate.notify_one();
    let (st, _, b) = leader.await.unwrap();
    assert_eq!(st, "edge; fwd=miss; fwd-status=200; stored; ttl=60");
    assert_eq!(b, "shared");
    for f in followers {
        let (st, _, b) = f.await.unwrap();
        assert_eq!(
            st,
            "edge; fwd=miss; fwd-status=200; stored; collapsed; ttl=60"
        );
        assert_eq!(b, "shared");
    }
    assert_eq!(h.upstream.calls(), 1);
    assert_eq!(h.layer.in_flight(), 0);
}

#[tokio::test]
async fn a_shared_revalidation_reaches_its_followers() {
    let h = Arc::new(harness());
    h.upstream.push(origin(
        200,
        &[("cache-control", "max-age=1"), ("etag", "\"r\"")],
        "rv",
    ));
    h.get("/rv").await;
    h.clock.advance(5);
    let gate = h.upstream.gated(Script::Respond(origin(
        304,
        &[("cache-control", "max-age=60"), ("etag", "\"r\"")],
        "",
    )));
    let leader = spawn_get(&h, "/rv", &[]);
    until(|| h.layer.in_flight() == 1).await;
    let follower = spawn_get(&h, "/rv", &[]);
    settle().await;
    gate.notify_one();
    leader.await.unwrap();
    let (st, _, b) = follower.await.unwrap();
    assert_eq!(
        st,
        "edge; fwd=stale; fwd-status=304; stored; collapsed; ttl=60"
    );
    assert_eq!(b, "rv");
    assert_eq!(h.upstream.calls(), 2);
}

#[tokio::test]
async fn followers_go_alone_when_the_result_was_only_for_the_leader() {
    let h = Arc::new(harness());
    // Not storable.
    let gate = h.upstream.gated(Script::Respond(origin(
        200,
        &[("cache-control", "private")],
        "mine",
    )));
    h.upstream
        .push(origin(200, &[("cache-control", "private")], "yours"));
    let leader = spawn_get(&h, "/p", &[]);
    until(|| h.layer.in_flight() == 1).await;
    let follower = spawn_get(&h, "/p", &[]);
    settle().await;
    gate.notify_one();
    assert_eq!(leader.await.unwrap().2, "mine");
    let (st, _, b) = follower.await.unwrap();
    assert_eq!(st, "edge; fwd=miss; fwd-status=200");
    assert_eq!(b, "yours");
    // A variant the follower's request does not select.
    let gate = h.upstream.gated(Script::Respond(origin(
        200,
        &[("cache-control", "max-age=60"), ("vary", "accept-language")],
        "en",
    )));
    h.upstream.push(origin(
        200,
        &[("cache-control", "max-age=60"), ("vary", "accept-language")],
        "fr",
    ));
    let leader = spawn_get(&h, "/v", &[("accept-language", "en")]);
    until(|| h.layer.in_flight() == 1).await;
    let follower = spawn_get(&h, "/v", &[("accept-language", "fr")]);
    settle().await;
    gate.notify_one();
    assert_eq!(leader.await.unwrap().2, "en");
    let (st, _, b) = follower.await.unwrap();
    assert_eq!(st, "edge; fwd=miss; fwd-status=200; stored; ttl=60");
    assert_eq!(b, "fr");
    assert_eq!(h.upstream.calls(), 4);
}

#[tokio::test]
async fn followers_share_a_failure_or_its_stale_copy() {
    let h = Arc::new(harness());
    let gate = h.upstream.gated(Script::Fail("connect"));
    let leader = spawn_get(&h, "/f", &[]);
    until(|| h.layer.in_flight() == 1).await;
    let follower = spawn_get(&h, "/f", &[]);
    settle().await;
    gate.notify_one();
    let (st, code, _) = leader.await.unwrap();
    assert_eq!(
        (st.as_str(), code),
        ("edge; fwd=miss; detail=connect", StatusCode::BAD_GATEWAY)
    );
    let (st, code, b) = follower.await.unwrap();
    assert_eq!(st, "edge; fwd=miss; collapsed; detail=connect");
    assert_eq!(code, StatusCode::BAD_GATEWAY);
    assert_eq!(b, "origin unreachable\n");
    // With a copy stale-if-error allows, everyone gets the copy.
    h.upstream.push(origin(
        200,
        &[("cache-control", "max-age=1, stale-if-error=60")],
        "old",
    ));
    h.get("/f").await;
    h.clock.advance(5);
    let gate = h.upstream.gated(Script::Respond(origin(503, &[], "down")));
    let leader = spawn_get(&h, "/f", &[]);
    until(|| h.layer.in_flight() == 1).await;
    let follower = spawn_get(&h, "/f", &[]);
    settle().await;
    gate.notify_one();
    for t in [leader, follower] {
        let (st, code, b) = t.await.unwrap();
        assert_eq!(
            st,
            "edge; hit; ttl=-4; fwd=stale; fwd-status=503; detail=stale-if-error"
        );
        assert_eq!((code, b.as_ref()), (StatusCode::OK, &b"old"[..]));
    }
    assert_eq!(h.upstream.calls(), 3);
}

#[tokio::test]
async fn a_refused_request_is_not_an_upstream_failure() {
    let h = Arc::new(harness());
    h.upstream.push(origin(
        200,
        &[("cache-control", "max-age=1, stale-if-error=60")],
        "old",
    ));
    h.get("/b").await;
    h.clock.advance(5);
    let gate = h.upstream.gated(Script::Refuse);
    h.upstream
        .push(origin(200, &[("cache-control", "max-age=60")], "new"));
    let leader = spawn_get(&h, "/b", &[]);
    until(|| h.layer.in_flight() == 1).await;
    let follower = spawn_get(&h, "/b", &[]);
    settle().await;
    gate.notify_one();
    // The refusal is about the leader's request: no stale copy for it, and
    // the follower makes its own request.
    let (st, code, _) = leader.await.unwrap();
    assert_eq!(
        (st.as_str(), code),
        (
            "edge; fwd=stale; detail=bad_request",
            StatusCode::BAD_REQUEST
        )
    );
    let (st, _, b) = follower.await.unwrap();
    assert_eq!(st, "edge; fwd=stale; fwd-status=200; stored; ttl=60");
    assert_eq!(b, "new");
}

#[tokio::test]
async fn a_leader_that_goes_away_hands_over_to_a_follower() {
    let h = Arc::new(harness());
    let _gate = h.upstream.gated(Script::Respond(origin(
        200,
        &[("cache-control", "max-age=60")],
        "never",
    )));
    h.upstream.push(origin(
        200,
        &[("cache-control", "max-age=60")],
        "taken over",
    ));
    let leader = spawn_get(&h, "/a", &[]);
    until(|| h.layer.in_flight() == 1).await;
    let follower = spawn_get(&h, "/a", &[]);
    settle().await;
    leader.abort();
    let (st, _, b) = follower.await.unwrap();
    assert_eq!(st, "edge; fwd=miss; fwd-status=200; stored; ttl=60");
    assert_eq!(b, "taken over");
    assert_eq!(h.upstream.calls(), 2);
}

#[tokio::test]
async fn unsafe_methods_invalidate_what_they_name() {
    let h = harness();
    for path in ["/u", "/loc", "/cl"] {
        h.upstream
            .push(origin(200, &[("cache-control", "max-age=60")], "x"));
        h.get(path).await;
    }
    assert_eq!(h.layer.cache().memory().len(), 3);
    h.upstream.push(origin(
        201,
        &[
            ("location", "/loc"),
            ("content-location", "http://A/cl"),
            ("surrogate-key", "s"),
        ],
        "",
    ));
    let resp = h.send("POST", "/u", &[]).await;
    assert_eq!(status(&resp), "edge; fwd=method; fwd-status=201");
    assert!(!resp.headers().contains_key("surrogate-key"));
    assert_eq!(h.layer.cache().memory().len(), 0);
    // A failed unsafe request invalidates nothing; one on another host
    // only its own URL.
    h.upstream
        .push(origin(200, &[("cache-control", "max-age=60")], "x"));
    h.get("/u").await;
    h.upstream.push(origin(500, &[], ""));
    h.send("DELETE", "/u", &[]).await;
    assert_eq!(h.layer.cache().memory().len(), 1);
    h.upstream
        .push(origin(204, &[("location", "http://elsewhere/u")], ""));
    h.send("PUT", "/x", &[]).await;
    assert_eq!(h.layer.cache().memory().len(), 1);
    h.upstream
        .push(origin(204, &[("location", "relative")], ""));
    h.send("PUT", "/u", &[]).await;
    assert_eq!(h.layer.cache().memory().len(), 0);
    // A proxy failure on a pass is reported with its reason.
    h.upstream.fail("connect");
    let resp = h.send("POST", "/u", &[]).await;
    assert_eq!(status(&resp), "edge; fwd=method; detail=connect");
}

#[tokio::test]
async fn route_overrides_apply() {
    let h = harness_with(
        "max_variants = 4\n\
         routes = [{ host = \"a\", path_prefix = \"/\", max_ttl_secs = 30, \
         heuristic_max_secs = 5, max_variants = 1 }]\n",
        Duration::from_secs(5),
    );
    h.upstream
        .push(origin(200, &[("cache-control", "max-age=3600")], "capped"));
    let resp = h.get("/t").await;
    assert!(
        status(&resp).ends_with("stored; ttl=30"),
        "{}",
        status(&resp)
    );
    // Heuristic freshness from Last-Modified a day ago, capped at 5 s.
    let lm = http_date(h.clock.now() - Duration::from_secs(86_400));
    h.upstream.push(origin(
        200,
        &[("last-modified", lm.to_str().unwrap()), ("date", &h.date())],
        "guessed",
    ));
    let resp = h.get("/h").await;
    assert!(
        status(&resp).ends_with("stored; ttl=5"),
        "{}",
        status(&resp)
    );
    // One variant per URL on this route.
    for lang in ["en", "fr"] {
        h.upstream.push(origin(
            200,
            &[("cache-control", "max-age=60"), ("vary", "accept-language")],
            "v",
        ));
        h.send("GET", "/v", &[("accept-language", lang)]).await;
    }
    let resp = h.send("GET", "/v", &[("accept-language", "fr")]).await;
    assert_eq!(status(&resp), "edge; hit; ttl=30");
    h.upstream.push(origin(
        200,
        &[("cache-control", "max-age=60"), ("vary", "accept-language")],
        "v",
    ));
    let resp = h.send("GET", "/v", &[("accept-language", "en")]).await;
    assert!(
        status(&resp).starts_with("edge; fwd=miss"),
        "en was dropped"
    );
}

#[tokio::test]
async fn a_purge_during_the_fetch_wins() {
    let h = harness();
    let filters = h.layer.route_filters();
    let req = Request::get("/race").header("host", "a").body(()).unwrap();
    let cache = h.layer.cache().clone();
    let resp = filter::run(filters.get(0), 0, req, move |_| {
        let cache = cache.clone();
        async move {
            cache.purge(&[PurgeTarget::Key("a/race".into())]).await;
            (
                origin(200, &[("cache-control", "max-age=60")], "late"),
                Delivery::Upstream,
            )
        }
    })
    .await;
    assert_eq!(status(&resp), "edge; fwd=miss; fwd-status=200; ttl=60");
    assert_eq!(body(resp).await, "late");
    assert_eq!(h.layer.cache().memory().len(), 0);
}

#[test]
fn settings_and_names() {
    let d = LayerSettings::default();
    assert_eq!(d.name, "edgefront");
    assert_eq!(d.body_timeout, Duration::from_secs(30));
    let cache = Arc::new(Cache::new(&CacheSettings::default()).unwrap());
    let layer = CacheLayer::new(
        cache,
        Default::default(),
        LayerSettings {
            name: "not a token".into(),
            ..LayerSettings::default()
        },
        Arc::new(SystemClock),
    );
    assert_eq!(layer.name(), "edgefront");
    assert_eq!(layer.route_filters().filtered(), 0);
    let now = SystemClock.now();
    assert!(now > UNIX_EPOCH);
}

#[tokio::test]
async fn prefixed_bodies_report_their_size() {
    let body = Prefixed {
        prefix: Some(Bytes::from_static(b"ab")),
        rest: full("cd"),
    };
    assert!(!body.is_end_stream());
    assert_eq!(body.size_hint().exact(), Some(4));
    let all = body.collect().await.unwrap().to_bytes();
    assert_eq!(all, "abcd");
    let body = Prefixed {
        prefix: None,
        rest: full(""),
    };
    assert!(body.is_end_stream());
    let chunks = Prefixed {
        prefix: Some(Bytes::from_static(b"x")),
        rest: Chunks(VecDeque::from(["y"])),
    };
    assert_eq!(chunks.size_hint().lower(), 1);
    assert_eq!(chunks.size_hint().upper(), None);
}

/// Records what the layer reports.
#[derive(Default)]
struct Events(Mutex<Vec<String>>);

impl Observer for Events {
    fn not_stored(&self, reason: edgefront_cache::policy::NotStorable) {
        self.0
            .lock()
            .unwrap()
            .push(format!("not_stored {}", reason.as_str()));
    }
    fn streamed_uncached(&self) {
        self.0.lock().unwrap().push("streamed_uncached".into());
    }
    fn collapsed(&self) {
        self.0.lock().unwrap().push("collapsed".into());
    }
    fn background_refresh(&self) {
        self.0.lock().unwrap().push("background_refresh".into());
    }
}

#[tokio::test]
async fn the_observer_hears_what_responses_do_not_show() {
    let mut h = harness_with(
        "routes = [{ host = \"a\", path_prefix = \"/\", max_object_bytes = 4 }]\n",
        Duration::from_secs(5),
    );
    let events = Arc::new(Events::default());
    let cfg: CacheConfig =
        toml::from_str("routes = [{ host = \"a\", path_prefix = \"/\", max_object_bytes = 4 }]")
            .unwrap();
    h.layer = CacheLayer::with_observer(
        h.layer.cache().clone(),
        cfg.resolve(&[route("a", "/"), route("b", "/")]).unwrap(),
        LayerSettings::default(),
        h.clock.clone(),
        events.clone(),
    );
    let h = Arc::new(h);
    h.upstream
        .push(origin(200, &[("cache-control", "private")], "p"));
    h.get("/p").await;
    h.upstream.push(origin(
        200,
        &[("cache-control", "max-age=60"), ("content-length", "6")],
        "123456",
    ));
    h.get("/big").await;
    let gate = h.upstream.gated(Script::Respond(origin(
        200,
        &[("cache-control", "max-age=1, stale-while-revalidate=60")],
        "c",
    )));
    let leader = spawn_get(&h, "/c", &[]);
    until(|| h.layer.in_flight() == 1).await;
    let follower = spawn_get(&h, "/c", &[]);
    settle().await;
    gate.notify_one();
    leader.await.unwrap();
    let (st, _, _) = follower.await.unwrap();
    assert!(st.contains("collapsed"), "{st}");
    h.clock.advance(5);
    h.upstream
        .push(origin(200, &[("cache-control", "max-age=60")], "c2"));
    let resp = h.get("/c").await;
    assert_eq!(resp.extensions().get::<Outcome>(), Some(&Outcome::Stale));
    until(|| h.layer.in_flight() == 0 && h.upstream.calls() == 4).await;
    assert_eq!(
        *events.0.lock().unwrap(),
        [
            "not_stored private",
            "streamed_uncached",
            "collapsed",
            "background_refresh"
        ]
    );
    // The default observer hears nothing and costs nothing.
    struct Silent;
    impl Observer for Silent {}
    let s = Silent;
    s.not_stored(edgefront_cache::policy::NotStorable::Private);
    s.streamed_uncached();
    s.collapsed();
    s.background_refresh();
}

#[tokio::test]
async fn every_answer_carries_its_outcome() {
    let h = harness();
    let outcome = |r: &Response<Body>| *r.extensions().get::<Outcome>().unwrap();
    h.upstream.push(origin(
        200,
        &[
            ("cache-control", "max-age=1, stale-if-error=600"),
            ("etag", "\"o\""),
        ],
        "o",
    ));
    assert_eq!(outcome(&h.get("/o").await), Outcome::Miss);
    assert_eq!(outcome(&h.get("/o").await), Outcome::Hit);
    h.clock.advance(5);
    let r = h
        .send("GET", "/o", &[("cache-control", "max-stale=60")])
        .await;
    assert_eq!(outcome(&r), Outcome::Stale);
    // A HEAD from that copy is a plain hit, as cachet 0.1 reported it.
    let r = h
        .send("HEAD", "/o", &[("cache-control", "max-stale=60")])
        .await;
    assert_eq!(outcome(&r), Outcome::Hit);
    assert_eq!(status(&r), "edge; hit; ttl=-4");
    h.upstream.push(origin(304, &[("etag", "\"o\"")], ""));
    assert_eq!(outcome(&h.get("/o").await), Outcome::Revalidated);
    h.clock.advance(5);
    h.upstream.fail("connect");
    assert_eq!(outcome(&h.get("/o").await), Outcome::StaleIfError);
    h.upstream.push(origin(200, &[], ""));
    let r = h.send("POST", "/o", &[]).await;
    assert_eq!(outcome(&r), Outcome::Pass);
    // Responses that pass through get the layer's Via.
    assert_eq!(r.headers()["via"], "1.1 edge");
    h.upstream.fail("connect");
    assert_eq!(outcome(&h.send("POST", "/o", &[]).await), Outcome::Error);
    let r = h
        .send("GET", "/none", &[("cache-control", "only-if-cached")])
        .await;
    assert_eq!(outcome(&r), Outcome::Error);
    h.upstream.fail("timeout");
    assert_eq!(outcome(&h.get("/none").await), Outcome::Error);
    let names: Vec<_> = Outcome::ALL.iter().map(|o| o.as_str()).collect();
    assert_eq!(
        names,
        [
            "hit",
            "stale",
            "stale_if_error",
            "miss",
            "revalidated",
            "pass",
            "error"
        ]
    );
}
