//! Filter phases in the proxy's request path, over real TCP: where each
//! phase runs, what it sees, and how it combines with the cluster's limits
//! and the proxy's own errors.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use edgefront_core::body::{Body, full, text};
use edgefront_core::config::Config;
use edgefront_core::filter::{
    BoxFuture, Decision, Delivery, Exchange, Filter, RequestHead, RouteFilters, Upstream,
};
use edgefront_core::listener::{ServerLimits, serve};
use edgefront_proxy::server::Proxy;
use http::{HeaderValue, Response, StatusCode};
use http_body_util::BodyExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

type Log = Arc<Mutex<Vec<String>>>;

/// Answers `/hit` itself, answers `/refresh` and refreshes it in the
/// background, and forwards the rest with an exchange that records what it
/// sees and marks what it delivers.
struct Probe {
    log: Log,
}

struct Recorder {
    log: Log,
}

impl Filter for Probe {
    fn request<'a>(&'a self, req: RequestHead<'a>) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            self.log
                .lock()
                .unwrap()
                .push(format!("request {} {}", req.route, req.uri.path()));
            req.headers
                .insert("x-filtered", HeaderValue::from_static("yes"));
            let exchange = Box::new(Recorder {
                log: self.log.clone(),
            });
            match req.uri.path() {
                "/hit" => Decision::Respond(text(StatusCode::OK, "hit\n")),
                "/refresh" => Decision::Refresh(text(StatusCode::OK, "stale\n"), exchange),
                _ => Decision::Forward(Some(exchange)),
            }
        })
    }
}

impl Exchange for Recorder {
    fn upstream_response(&mut self, resp: Response<Body>) -> BoxFuture<'_, Upstream> {
        Box::pin(async move {
            let (parts, body) = resp.into_parts();
            // The proxy adds the request ID after this phase, so a filter
            // that stores the response does not store the ID with it.
            self.log.lock().unwrap().push(format!(
                "upstream {} request-id={}",
                parts.status.as_u16(),
                parts.headers.contains_key("x-request-id")
            ));
            if parts.headers.contains_key("x-resend") {
                let mut h = http::HeaderMap::new();
                h.insert("x-resent", HeaderValue::from_static("1"));
                h.insert("host", HeaderValue::from_static("a"));
                return Upstream::Resend(h);
            }
            let mut bytes = body.collect().await.unwrap().to_bytes().to_vec();
            bytes.push(b'!');
            let mut parts = parts;
            parts.headers.remove("content-length");
            Upstream::Respond(Response::from_parts(parts, full(bytes)))
        })
    }

    fn deliver(self: Box<Self>, mut resp: Response<Body>, from: Delivery) -> Response<Body> {
        let phase = match from {
            Delivery::Upstream => "upstream".to_owned(),
            Delivery::Generated(reason) => format!("generated:{reason}"),
        };
        self.log.lock().unwrap().push(format!("deliver {phase}"));
        if from == Delivery::Generated("upstream_timeout") {
            resp = text(StatusCode::OK, "rescued\n");
        }
        resp.headers_mut()
            .insert("x-phase", HeaderValue::from_str(&phase).unwrap());
        resp
    }
}

struct Running {
    addr: SocketAddr,
    log: Log,
    _stop: oneshot::Sender<()>,
}

/// A proxy with the probe on route 0 (`/`) and nothing on route 1
/// (`/plain/`).
async fn start(endpoints: &[&str], extra: &str) -> Running {
    let eps: Vec<String> = endpoints
        .iter()
        .map(|a| format!("{{\"address\":\"{a}\"}}"))
        .collect();
    let config = format!(
        "{{\"listen\":\"127.0.0.1:0\",\"routes\":[\
         {{\"host\":\"*\",\"path_prefix\":\"/\",\"cluster\":\"web\"}},\
         {{\"host\":\"*\",\"path_prefix\":\"/plain/\",\"cluster\":\"web\"}}],\
         \"clusters\":[{{\"name\":\"web\",\"endpoints\":[{}]{extra}}}]}}",
        eps.join(",")
    );
    let cfg = Config::from_json(config.as_bytes()).expect("a valid config");
    let log = Log::default();
    let mut filters = RouteFilters::new(2);
    filters.set(0, Arc::new(Probe { log: log.clone() }));
    let proxy = Arc::new(Proxy::new(&cfg).with_filters(filters));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(serve(
        listener,
        proxy,
        ServerLimits::default(),
        async move {
            let _ = stopped.await;
        },
    ));
    Running {
        addr,
        log,
        _stop: stop,
    }
}

/// An upstream that reports each request head, then answers `reply`
/// after `delay`.
async fn upstream(
    reply: &'static [u8],
    delay: Duration,
) -> (String, mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let (r, mut w) = conn.into_split();
                let mut r = BufReader::new(r);
                loop {
                    let mut head = String::new();
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push_str(&line);
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let _ = tx.send(head);
                    tokio::time::sleep(delay).await;
                    if w.write_all(reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (addr, rx)
}

const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

async fn get(addr: SocketAddr, path: &str) -> String {
    let mut conn = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
    conn.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

fn log_of(r: &Running) -> Vec<String> {
    r.log.lock().unwrap().clone()
}

#[tokio::test]
async fn forwarded_requests_go_through_every_phase() {
    let (up, mut seen) = upstream(OK, Duration::ZERO).await;
    let proxy = start(&[&up], "").await;
    let resp = get(proxy.addr, "/x").await;
    assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "{resp}");
    assert!(resp.ends_with("\r\n\r\nok!"), "{resp}");
    assert!(resp.contains("x-phase: upstream\r\n"), "{resp}");
    assert!(resp.contains("x-request-id: "), "{resp}");
    let head = seen.recv().await.unwrap();
    assert!(head.contains("x-filtered: yes\r\n"), "{head}");
    assert_eq!(
        log_of(&proxy),
        [
            "request 0 /x",
            "upstream 200 request-id=false",
            "deliver upstream"
        ]
    );
}

#[tokio::test]
async fn routes_without_a_filter_skip_the_phases() {
    let (up, mut seen) = upstream(OK, Duration::ZERO).await;
    let proxy = start(&[&up], "").await;
    let resp = get(proxy.addr, "/plain/x").await;
    assert!(resp.ends_with("\r\n\r\nok"), "{resp}");
    assert!(!resp.contains("x-phase"), "{resp}");
    let head = seen.recv().await.unwrap();
    assert!(!head.contains("x-filtered"), "{head}");
    assert!(log_of(&proxy).is_empty());
}

#[tokio::test]
async fn an_answer_from_the_request_phase_needs_no_upstream_or_permit() {
    // The only endpoint holds a request for a second, and the cluster
    // allows one at a time.
    let (up, mut seen) = upstream(OK, Duration::from_secs(1)).await;
    let proxy = start(&[&up], r#","max_requests":1"#).await;
    let addr = proxy.addr;
    let busy = tokio::spawn(async move { get(addr, "/slow").await });
    seen.recv().await.unwrap();
    let hit = get(proxy.addr, "/hit").await;
    assert!(hit.starts_with("HTTP/1.1 200 OK\r\n"), "{hit}");
    assert!(hit.ends_with("hit\n"), "{hit}");
    assert!(hit.contains("x-request-id: "), "{hit}");
    // A forwarded request is refused, and the refusal is delivered.
    let refused = get(proxy.addr, "/other").await;
    assert!(
        refused.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "{refused}"
    );
    assert!(
        refused.contains("x-phase: generated:overloaded\r\n"),
        "{refused}"
    );
    assert!(refused.contains("x-steer-overflow: cluster_max_requests\r\n"));
    assert!(busy.await.unwrap().ends_with("ok!"));
    assert!(seen.try_recv().is_err(), "the hit never went upstream");
}

#[tokio::test]
async fn the_proxys_errors_reach_the_deliver_phase() {
    // Nothing listens on the endpoint.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = closed.local_addr().unwrap().to_string();
    drop(closed);
    let proxy = start(&[&dead], "").await;
    let resp = get(proxy.addr, "/x").await;
    assert!(resp.starts_with("HTTP/1.1 502 Bad Gateway\r\n"), "{resp}");
    assert!(
        resp.contains("x-phase: generated:bad_gateway\r\n"),
        "{resp}"
    );
    assert!(resp.contains("\"code\":\"bad_gateway\""), "{resp}");
    // A timeout the deliver phase turns into an answer of its own keeps
    // the request ID.
    let (up, _seen) = upstream(OK, Duration::from_secs(2)).await;
    let proxy = start(&[&up], r#","timeout":"100ms""#).await;
    let resp = get(proxy.addr, "/x").await;
    assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "{resp}");
    assert!(resp.ends_with("rescued\n"), "{resp}");
    assert!(resp.contains("x-request-id: "), "{resp}");
    assert!(resp.contains("x-phase: generated:upstream_timeout\r\n"));
}

#[tokio::test]
async fn a_refresh_answers_at_once_and_goes_upstream_in_the_background() {
    let (up, mut seen) = upstream(OK, Duration::from_millis(200)).await;
    let proxy = start(&[&up], "").await;
    let started = std::time::Instant::now();
    let resp = get(proxy.addr, "/refresh?q=1").await;
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "not held up"
    );
    assert!(resp.ends_with("stale\n"), "{resp}");
    let id = resp
        .split("x-request-id: ")
        .nth(1)
        .unwrap()
        .split("\r\n")
        .next()
        .unwrap()
        .to_owned();
    let head = seen.recv().await.unwrap();
    assert!(head.starts_with("GET /refresh?q=1 HTTP/1.1\r\n"), "{head}");
    assert!(head.contains("x-filtered: yes\r\n"), "{head}");
    assert!(head.contains(&format!("x-request-id: {id}\r\n")), "{head}");
    assert!(
        !head.to_ascii_lowercase().contains("content-length"),
        "{head}"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while log_of(&proxy).len() < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        log_of(&proxy),
        [
            "request 0 /refresh",
            "upstream 200 request-id=false",
            "deliver upstream"
        ]
    );
}

#[tokio::test]
async fn a_refresh_reports_failures_to_its_exchange() {
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = closed.local_addr().unwrap().to_string();
    drop(closed);
    let proxy = start(&[&dead], "").await;
    let resp = get(proxy.addr, "/refresh").await;
    assert!(resp.ends_with("stale\n"), "{resp}");
    tokio::time::timeout(Duration::from_secs(5), async {
        while log_of(&proxy).len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        log_of(&proxy),
        ["request 0 /refresh", "deliver generated:bad_gateway"]
    );
    // And a refresh over the cluster's limit is refused like a request.
    let (up, mut seen) = upstream(OK, Duration::from_secs(1)).await;
    let proxy = start(&[&up], r#","max_requests":1"#).await;
    let addr = proxy.addr;
    let busy = tokio::spawn(async move { get(addr, "/slow").await });
    seen.recv().await.unwrap();
    let resp = get(proxy.addr, "/refresh").await;
    assert!(resp.ends_with("stale\n"), "{resp}");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !log_of(&proxy).contains(&"deliver generated:overloaded".to_owned()) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    busy.await.unwrap();
}

#[tokio::test]
async fn a_refresh_is_held_to_the_cluster_timeout() {
    let (up, _seen) = upstream(OK, Duration::from_secs(2)).await;
    let proxy = start(&[&up], r#","timeout":"100ms""#).await;
    let resp = get(proxy.addr, "/refresh").await;
    assert!(resp.ends_with("stale\n"), "{resp}");
    tokio::time::timeout(Duration::from_secs(5), async {
        while log_of(&proxy).len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        log_of(&proxy),
        ["request 0 /refresh", "deliver generated:upstream_timeout"]
    );
}

/// An upstream that asks for a resend (`X-Resend`) unless the request is
/// one (`x-resent`), or always when `always` is set.
async fn resending(always: bool) -> (String, mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let (r, mut w) = conn.into_split();
                let mut r = BufReader::new(r);
                loop {
                    let mut head = String::new();
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        if let Some(v) = line.strip_prefix("content-length:") {
                            length = v.trim().parse().unwrap();
                        }
                        head.push_str(&line);
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let mut body = vec![0; length];
                    if r.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    let resend = always || !head.contains("x-resent: 1\r\n");
                    let _ = tx.send(head);
                    let reply: &[u8] = if resend {
                        b"HTTP/1.1 304 Not Modified\r\nX-Resend: 1\r\n\r\n"
                    } else {
                        OK
                    };
                    if w.write_all(reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (addr, rx)
}

#[tokio::test]
async fn a_resend_goes_upstream_once_more_without_a_body() {
    let (up, mut seen) = resending(false).await;
    let proxy = start(&[&up], "").await;
    let mut conn = TcpStream::connect(proxy.addr).await.unwrap();
    conn.write_all(
        b"POST /r HTTP/1.1\r\nHost: a\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody",
    )
    .await
    .unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.read_to_end(&mut out)).await;
    let resp = String::from_utf8_lossy(&out).into_owned();
    assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "{resp}");
    assert!(resp.ends_with("ok!"), "{resp}");
    let first = seen.recv().await.unwrap();
    assert!(first.contains("content-length: 4\r\n"), "{first}");
    let second = seen.recv().await.unwrap();
    assert!(second.starts_with("POST /r HTTP/1.1\r\n"), "{second}");
    assert!(second.contains("x-resent: 1\r\n"), "{second}");
    assert!(second.contains("x-request-id: "), "{second}");
    assert!(
        second.contains("x-forwarded-for: 127.0.0.1\r\n"),
        "{second}"
    );
    assert!(
        !second.contains("x-filtered"),
        "the resend's headers replace the first's"
    );
    assert!(second.contains("content-length: 0\r\n"), "{second}");
}

#[tokio::test]
async fn a_second_resend_is_refused() {
    let (up, mut seen) = resending(true).await;
    let proxy = start(&[&up], "").await;
    let resp = get(proxy.addr, "/r").await;
    assert!(resp.starts_with("HTTP/1.1 502 Bad Gateway\r\n"), "{resp}");
    assert!(
        resp.contains("x-phase: generated:resend_limit\r\n"),
        "{resp}"
    );
    seen.recv().await.unwrap();
    seen.recv().await.unwrap();
    assert!(seen.try_recv().is_err(), "two requests, not three");
    // In a background refresh too.
    let resp = get(proxy.addr, "/refresh").await;
    assert!(resp.ends_with("stale\n"), "{resp}");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !log_of(&proxy).contains(&"deliver generated:resend_limit".to_owned())
            || log_of(&proxy)
                .iter()
                .filter(|l| l.contains("resend_limit"))
                .count()
                < 2
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_resend_does_not_hold_two_places() {
    // One place in the cluster: the first try gives it back before the
    // resend takes it.
    let (up, _seen) = resending(false).await;
    let proxy = start(&[&up], r#","max_requests":1"#).await;
    let resp = get(proxy.addr, "/r").await;
    assert!(resp.ends_with("ok!"), "{resp}");
}
