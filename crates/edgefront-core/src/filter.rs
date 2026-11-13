//! Filter phases: how a layer such as the cache takes part in the proxy's
//! request path without either crate depending on the other.
//!
//! A [`Filter`] is attached to a route when the proxy is built. A route
//! with no filter skips all of this, so it pays one branch per request.
//! For a route with a filter, each request goes through up to three
//! phases:
//!
//! 1. **Request**, after routing and before the upstream is picked.
//!    [`Filter::request`] sees the route index, method, URI and headers,
//!    and may change the headers that go upstream. It either answers the
//!    request itself ([`Decision::Respond`], a cache hit) or lets it go on
//!    ([`Decision::Forward`]). It can hand back an [`Exchange`] that holds
//!    this request's state for the next two phases.
//! 2. **Upstream response**, once the upstream's response head has arrived
//!    and before any of it reaches the client.
//!    [`Exchange::upstream_response`] gets the head and the body stream,
//!    and returns the response to send on. That can be the same response,
//!    a stored object for a `304`, or a body the filter has read into its
//!    store.
//! 3. **Deliver**, once on the response the client gets, whether it came
//!    from upstream or the proxy wrote it (`502`, `504`, `503`).
//!    [`Exchange::deliver`] may add headers, or replace an error with
//!    something better, such as a stale copy that `stale-if-error`
//!    allows. [`Delivery`] says where the response came from.
//!
//! A response the request phase answered with is final: it is not passed
//! to the deliver phase, because the filter that wrote it wrote all of it.
//!
//! The request phase can also answer at once and still have the request
//! sent upstream in the background ([`Decision::Refresh`]), which is how
//! a cache serves a stale copy while it refreshes it. The background
//! exchange goes through the same two later phases, and the response its
//! deliver phase returns is dropped.
//!
//! [`run`] drives the phases around an upstream call. A server with steps
//! of its own between the phases can call them directly instead, keeping
//! the same order.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use http::{HeaderMap, Method, Request, Response, Uri};

use crate::body::Body;

/// A future a filter phase returns. Phases are called through trait
/// objects, so their futures are boxed.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The request as the request phase sees it.
#[derive(Debug)]
pub struct RequestHead<'a> {
    /// The index of the route the request matched, in configuration order.
    pub route: usize,
    pub method: &'a Method,
    pub uri: &'a Uri,
    /// The request's headers. Changes here are what goes upstream.
    pub headers: &'a mut HeaderMap,
}

/// What the request phase decided.
pub enum Decision {
    /// Send the request upstream. The exchange, if any, takes the
    /// upstream response and delivery phases for this request.
    Forward(Option<Box<dyn Exchange>>),
    /// Answer with this response without going upstream. It is final.
    Respond(Response<Body>),
    /// Answer with this response now, and send the request upstream in
    /// the background for the exchange, whose delivered response is
    /// dropped.
    Refresh(Response<Body>, Box<dyn Exchange>),
}

impl std::fmt::Debug for Decision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Decision::Forward(x) => f
                .debug_tuple("Forward")
                .field(&x.as_ref().map(|_| "Exchange"))
                .finish(),
            Decision::Respond(r) => f.debug_tuple("Respond").field(&r.status()).finish(),
            Decision::Refresh(r, _) => f.debug_tuple("Refresh").field(&r.status()).finish(),
        }
    }
}

/// Where the response handed to [`Exchange::deliver`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// From upstream, after [`Exchange::upstream_response`].
    Upstream,
    /// Written by the proxy because there was no usable upstream response.
    /// The reason is a short token for logs and status headers, such as
    /// `connect`, `timeout`, `protocol`, `no_healthy_upstream` or
    /// `overloaded`.
    Generated(&'static str),
}

/// One request's state from the request phase on. It is dropped without
/// a call to [`Exchange::deliver`] only when the client went away first.
pub trait Exchange: Send {
    /// The upstream response, before any of it is sent on. Returns the
    /// response to deliver.
    fn upstream_response(&mut self, resp: Response<Body>) -> BoxFuture<'_, Response<Body>>;

    /// The response about to be written to the client. Called once.
    fn deliver(self: Box<Self>, resp: Response<Body>, from: Delivery) -> Response<Body>;
}

/// A layer in the proxy's request path, attached to the routes it serves.
pub trait Filter: Send + Sync {
    /// The request phase. See the [module docs](self).
    fn request<'a>(&'a self, req: RequestHead<'a>) -> BoxFuture<'a, Decision>;
}

impl<F: Filter + ?Sized> Filter for Arc<F> {
    fn request<'a>(&'a self, req: RequestHead<'a>) -> BoxFuture<'a, Decision> {
        (**self).request(req)
    }
}

/// The filter of each route, by route index.
#[derive(Clone, Default)]
pub struct RouteFilters {
    routes: Vec<Option<Arc<dyn Filter>>>,
}

impl std::fmt::Debug for RouteFilters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteFilters")
            .field("routes", &self.routes.len())
            .field("filtered", &self.filtered())
            .finish()
    }
}

impl RouteFilters {
    /// No filter on any of `routes` routes.
    pub fn new(routes: usize) -> Self {
        RouteFilters {
            routes: vec![None; routes],
        }
    }

    /// Attaches `filter` to route `index`, replacing any filter there.
    ///
    /// # Panics
    ///
    /// When `index` is not below the route count given to [`Self::new`].
    pub fn set(&mut self, index: usize, filter: Arc<dyn Filter>) {
        self.routes[index] = Some(filter);
    }

    /// The filter on route `index`, or `None` when the route has none or
    /// there is no such route.
    #[inline]
    pub fn get(&self, index: usize) -> Option<&dyn Filter> {
        self.routes.get(index)?.as_deref()
    }

    /// How many routes have a filter.
    pub fn filtered(&self) -> usize {
        self.routes.iter().filter(|f| f.is_some()).count()
    }
}

/// Runs `req` through the filter's phases around `send`, which makes the
/// upstream call and returns the response with where it came from: the
/// upstream's, or one the caller wrote because there was no usable
/// upstream response. Without a filter it is the response `send` returns.
/// A [`Decision::Refresh`] runs `send` on a task of its own, so this must
/// be called inside a tokio runtime.
pub async fn run<B, S, Fut>(
    filter: Option<&dyn Filter>,
    route: usize,
    req: Request<B>,
    send: S,
) -> Response<Body>
where
    B: Send + 'static,
    S: FnOnce(Request<B>) -> Fut + Send + 'static,
    Fut: Future<Output = (Response<Body>, Delivery)> + Send + 'static,
{
    let Some(filter) = filter else {
        return send(req).await.0;
    };
    let (mut parts, body) = req.into_parts();
    let decision = filter
        .request(RequestHead {
            route,
            method: &parts.method,
            uri: &parts.uri,
            headers: &mut parts.headers,
        })
        .await;
    let req = Request::from_parts(parts, body);
    match decision {
        Decision::Respond(resp) => resp,
        Decision::Forward(exchange) => complete(send(req).await, exchange).await,
        Decision::Refresh(resp, exchange) => {
            tokio::spawn(async move {
                drop(complete(send(req).await, Some(exchange)).await);
            });
            resp
        }
    }
}

/// The later two phases for what the upstream call returned.
async fn complete(
    sent: (Response<Body>, Delivery),
    exchange: Option<Box<dyn Exchange>>,
) -> Response<Body> {
    let Some(mut x) = exchange else {
        return sent.0;
    };
    let (resp, from) = match sent {
        (resp, Delivery::Upstream) => (x.upstream_response(resp).await, Delivery::Upstream),
        generated => generated,
    };
    x.deliver(resp, from)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use bytes::Bytes;
    use http::{HeaderValue, StatusCode};
    use http_body_util::BodyExt;

    use super::*;
    use crate::body::{full, text};

    /// Records every phase it sees, answers `/hit` itself, and tags the
    /// headers of what it forwards.
    #[derive(Default)]
    struct Recorder {
        log: Arc<Mutex<Vec<String>>>,
    }

    struct RecordingExchange {
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Filter for Recorder {
        fn request<'a>(&'a self, req: RequestHead<'a>) -> BoxFuture<'a, Decision> {
            Box::pin(async move {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("request {} {} {}", req.route, req.method, req.uri));
                match req.uri.path() {
                    "/hit" => Decision::Respond(text(StatusCode::OK, "from filter\n")),
                    "/plain" => Decision::Forward(None),
                    "/refresh" => Decision::Refresh(
                        text(StatusCode::OK, "stale\n"),
                        Box::new(RecordingExchange {
                            log: self.log.clone(),
                        }),
                    ),
                    _ => {
                        req.headers
                            .insert("x-filtered", HeaderValue::from_static("1"));
                        Decision::Forward(Some(Box::new(RecordingExchange {
                            log: self.log.clone(),
                        })))
                    }
                }
            })
        }
    }

    impl Exchange for RecordingExchange {
        fn upstream_response(&mut self, resp: Response<Body>) -> BoxFuture<'_, Response<Body>> {
            Box::pin(async move {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("upstream {}", resp.status().as_u16()));
                let (mut parts, body) = resp.into_parts();
                let bytes = body.collect().await.unwrap().to_bytes();
                parts
                    .headers
                    .insert("x-seen", HeaderValue::from(bytes.len()));
                Response::from_parts(parts, full(bytes))
            })
        }

        fn deliver(self: Box<Self>, mut resp: Response<Body>, from: Delivery) -> Response<Body> {
            self.log.lock().unwrap().push(format!("deliver {from:?}"));
            if from == Delivery::Generated("timeout") {
                return text(StatusCode::OK, "rescued\n");
            }
            resp.headers_mut()
                .insert("x-delivered", HeaderValue::from_static("1"));
            resp
        }
    }

    fn request(path: &str) -> Request<()> {
        Request::get(path).header("host", "a").body(()).unwrap()
    }

    async fn upstream(req: Request<()>) -> (Response<Body>, Delivery) {
        let filtered = req.headers().contains_key("x-filtered");
        if req.uri().path() == "/slow" {
            let resp = text(StatusCode::GATEWAY_TIMEOUT, "timed out\n");
            return (resp, Delivery::Generated("timeout"));
        }
        let mut resp = Response::new(full(Bytes::from_static(b"origin")));
        if filtered {
            resp.headers_mut()
                .insert("x-filtered", HeaderValue::from_static("1"));
        }
        (resp, Delivery::Upstream)
    }

    async fn body(resp: Response<Body>) -> Bytes {
        resp.into_body().collect().await.unwrap().to_bytes()
    }

    #[tokio::test]
    async fn without_a_filter_the_request_goes_straight_upstream() {
        let resp = run(None, 0, request("/x"), upstream).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(!resp.headers().contains_key("x-filtered"));
        assert_eq!(body(resp).await, "origin");
        let resp = run(None, 0, request("/slow"), upstream).await;
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test]
    async fn phases_run_in_order_and_see_each_others_changes() {
        let f = Recorder::default();
        let resp = run(Some(&f), 3, request("/x"), upstream).await;
        assert_eq!(resp.headers()["x-filtered"], "1", "upstream saw the header");
        assert_eq!(resp.headers()["x-seen"], "6");
        assert_eq!(resp.headers()["x-delivered"], "1");
        assert_eq!(body(resp).await, "origin");
        assert_eq!(
            *f.log.lock().unwrap(),
            ["request 3 GET /x", "upstream 200", "deliver Upstream"]
        );
    }

    #[tokio::test]
    async fn a_response_from_the_request_phase_is_final() {
        let f = Recorder::default();
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = called.clone();
        let resp = run(Some(&f), 0, request("/hit"), move |r| {
            c.store(true, std::sync::atomic::Ordering::Relaxed);
            upstream(r)
        })
        .await;
        assert!(
            !called.load(std::sync::atomic::Ordering::Relaxed),
            "upstream was not called"
        );
        assert_eq!(body(resp).await, "from filter\n");
        assert_eq!(*f.log.lock().unwrap(), ["request 0 GET /hit"]);
    }

    #[tokio::test]
    async fn generated_responses_reach_the_deliver_phase_only() {
        let f = Recorder::default();
        let resp = run(Some(&f), 1, request("/slow"), upstream).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body(resp).await, "rescued\n");
        assert_eq!(
            *f.log.lock().unwrap(),
            ["request 1 GET /slow", "deliver Generated(\"timeout\")"]
        );
    }

    #[tokio::test]
    async fn a_refresh_answers_at_once_and_runs_the_exchange_in_the_background() {
        let f = Recorder::default();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let resp = run(Some(&f), 2, request("/refresh"), |r| async move {
            // The client has its answer before the upstream is reached.
            rx.await.unwrap();
            upstream(r).await
        })
        .await;
        assert_eq!(body(resp).await, "stale\n");
        assert_eq!(*f.log.lock().unwrap(), ["request 2 GET /refresh"]);
        tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while f.log.lock().unwrap().len() < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            *f.log.lock().unwrap(),
            ["request 2 GET /refresh", "upstream 200", "deliver Upstream"]
        );
        let d = Decision::Refresh(
            text(StatusCode::OK, "x"),
            Box::new(RecordingExchange {
                log: Arc::default(),
            }),
        );
        assert_eq!(format!("{d:?}"), "Refresh(200)");
    }

    #[tokio::test]
    async fn forwarding_without_an_exchange_skips_the_later_phases() {
        let f = Recorder::default();
        let resp = run(Some(&f), 0, request("/plain"), upstream).await;
        assert!(!resp.headers().contains_key("x-delivered"));
        assert_eq!(body(resp).await, "origin");
        assert_eq!(*f.log.lock().unwrap(), ["request 0 GET /plain"]);
    }

    #[tokio::test]
    async fn the_request_keeps_its_version_and_extensions() {
        let f = Recorder::default();
        let mut req = request("/x");
        *req.version_mut() = http::Version::HTTP_10;
        req.extensions_mut().insert(7u32);
        let resp = run(Some(&f), 0, req, |r: Request<()>| async move {
            assert_eq!(r.version(), http::Version::HTTP_10);
            assert_eq!(r.extensions().get::<u32>(), Some(&7));
            assert_eq!(r.headers()["host"], "a");
            (Response::new(full("ok")), Delivery::Upstream)
        })
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn route_filters_index_by_route() {
        let mut fs = RouteFilters::new(3);
        assert_eq!(fs.filtered(), 0);
        assert!(fs.get(0).is_none());
        fs.set(1, Arc::new(Recorder::default()));
        assert!(fs.get(1).is_some());
        assert!(fs.get(2).is_none());
        assert!(fs.get(9).is_none(), "no such route");
        assert_eq!(fs.filtered(), 1);
        assert_eq!(format!("{fs:?}"), "RouteFilters { routes: 3, filtered: 1 }");
        let d = Decision::Forward(None);
        assert_eq!(format!("{d:?}"), "Forward(None)");
        let d = Decision::Forward(Some(Box::new(RecordingExchange {
            log: Arc::default(),
        })));
        assert_eq!(format!("{d:?}"), "Forward(Some(\"Exchange\"))");
        let d = Decision::Respond(text(StatusCode::OK, "x"));
        assert_eq!(format!("{d:?}"), "Respond(200)");
    }

    #[tokio::test]
    async fn an_arc_is_a_filter() {
        let f = Arc::new(Recorder::default());
        let resp = run(Some(&f), 0, request("/hit"), upstream).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
