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
    CACHE_STATUS, CacheLayer, Clock, LayerSettings, Prefixed, SystemClock, is_error_status,
};
use edgefront_cache::purge::PurgeTarget;
use edgefront_cache::store::{Cache, CacheSettings};
use edgefront_core::body::{Body, full, text};
use edgefront_core::config::Route;
use edgefront_core::filter::{self, Decision, Filter, Generated, RequestHead};
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper::body::{Body as HttpBody, Frame};

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
    Fail(&'static str),
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

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn last_headers(&self) -> HeaderMap {
        self.seen.lock().unwrap().last().unwrap().2.clone()
    }

    async fn send(&self, req: Request<()>) -> Result<Response<Body>, Generated> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.seen.lock().unwrap().push((
            req.method().clone(),
            req.uri().to_string(),
            req.headers().clone(),
        ));
        match self.script.lock().unwrap().pop_front() {
            Some(Script::Respond(r)) => Ok(r),
            Some(Script::Fail(reason)) => Err(Generated {
                reason,
                response: text(StatusCode::BAD_GATEWAY, "proxy error\n"),
            }),
            None => panic!("unexpected upstream request"),
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
    upstream: Upstream,
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
        },
        clock.clone(),
    );
    Harness {
        layer,
        upstream: Upstream::default(),
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
        filter::run(filters.get(0), 0, req.body(()).unwrap(), |r| {
            self.upstream.send(r)
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
async fn a_304_for_another_representation_drops_the_copy() {
    let h = harness();
    h.upstream.push(origin(
        200,
        &[("cache-control", "max-age=1"), ("etag", "\"v1\"")],
        "one",
    ));
    h.get("/m").await;
    h.clock.advance(5);
    h.upstream.push(origin(304, &[("etag", "\"v2\"")], ""));
    let resp = h.get("/m").await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(status(&resp), "edge; fwd=stale; detail=mismatched-304");
    h.upstream
        .push(origin(200, &[("cache-control", "max-age=60")], "two"));
    let resp = h.get("/m").await;
    assert_eq!(
        status(&resp),
        "edge; fwd=miss; fwd-status=200; stored; ttl=60"
    );
    assert!(!h.upstream.last_headers().contains_key("if-none-match"));
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

#[tokio::test]
async fn stale_while_revalidate_revalidates_before_use() {
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
    h.upstream.push(origin(
        304,
        &[("etag", "\"a\""), ("cache-control", "max-age=10")],
        "",
    ));
    let resp = h.get("/w").await;
    assert_eq!(
        status(&resp),
        "edge; fwd=stale; fwd-status=304; stored; ttl=10"
    );
    assert_eq!(body(resp).await, "a");
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
    let resp = filter::run(filters.get(0), 0, req, |_| async move {
        cache.purge(&[PurgeTarget::Key("a/race".into())]).await;
        Ok(origin(200, &[("cache-control", "max-age=60")], "late"))
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
