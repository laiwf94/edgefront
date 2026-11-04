//! The proxy's request path over real TCP: the answers steer gave when it
//! could not or would not forward, the cluster limits, and how bodies and
//! timeouts behave. Upstreams are scripted with hand-written HTTP/1.1, so
//! each test controls exactly what the proxy sees.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use edgefront_core::config::Config;
use edgefront_core::listener::{ServerLimits, serve};
use edgefront_proxy::server::Proxy;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

/// A proxy listening on a free port, stopped when dropped.
struct Running {
    addr: SocketAddr,
    _stop: oneshot::Sender<()>,
}

async fn start(config: &str) -> Running {
    let cfg = Config::from_json(config.as_bytes()).expect("a valid config");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let proxy = Arc::new(Proxy::new(&cfg));
    tokio::spawn(serve(
        listener,
        proxy,
        ServerLimits::default(),
        async move {
            let _ = stopped.await;
        },
    ));
    Running { addr, _stop: stop }
}

/// One cluster `web` behind `routes`, with `extra` cluster fields.
fn config(endpoints: &[&str], routes: &str, extra: &str) -> String {
    let eps: Vec<String> = endpoints
        .iter()
        .map(|a| format!("{{\"address\":\"{a}\"}}"))
        .collect();
    format!(
        "{{\"listen\":\"127.0.0.1:0\",\"routes\":[{routes}],\"clusters\":[{{\"name\":\"web\",\"endpoints\":[{}]{extra}}}]}}",
        eps.join(",")
    )
}

const ALL: &str = r#"{"host":"*","path_prefix":"/","cluster":"web"}"#;

/// An upstream that reads each request head (and a body by its length)
/// and reports it, then answers with `reply` after `delay`.
async fn upstream(
    reply: &'static [u8],
    delay: Duration,
) -> (String, mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((conn, _)) = listener.accept().await else {
                return;
            };
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
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
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
                    head.push_str(&String::from_utf8_lossy(&body));
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

/// Sends `request` on a new connection and reads until the proxy closes it
/// or `limit` passes.
async fn exchange(addr: SocketAddr, request: &[u8]) -> String {
    let mut conn = TcpStream::connect(addr).await.unwrap();
    conn.write_all(request).await.unwrap();
    read_all(&mut conn).await
}

async fn read_all(conn: &mut TcpStream) -> String {
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

fn request_id(resp: &str) -> &str {
    let at = resp.find("x-request-id: ").expect("a request id") + 14;
    resp[at..].split("\r\n").next().unwrap()
}

#[tokio::test]
async fn no_route_is_steers_404() {
    let (up, _) = upstream(OK, Duration::ZERO).await;
    let routes = r#"{"host":"api.example.com","path_prefix":"/v1/","cluster":"web"}"#;
    let proxy = start(&config(&[&up], routes, "")).await;
    let resp = exchange(
        proxy.addr,
        b"GET /v2/x HTTP/1.1\r\nHost: api.example.com\r\nX-Request-Id: rid-1\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 404 Not Found\r\n"), "{resp}");
    for h in [
        "content-type: application/json\r\n",
        "cache-control: no-store\r\n",
        "x-content-type-options: nosniff\r\n",
        "x-request-id: rid-1\r\n",
    ] {
        assert!(resp.contains(h), "{h} in {resp}");
    }
    assert!(resp.ends_with(
        "\r\n\r\n{\"error\":{\"code\":\"no_route\",\"message\":\"no route matches this host and path\",\"request_id\":\"rid-1\"}}\n"
    ));
}

#[tokio::test]
async fn max_requests_refuses_with_overflow_until_a_request_ends() {
    let (up, mut seen) = upstream(OK, Duration::from_millis(300)).await;
    let proxy = start(&config(&[&up], ALL, r#","max_requests":1"#)).await;
    let first = tokio::spawn(exchange(
        proxy.addr,
        b"GET /slow HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    ));
    seen.recv().await.unwrap();
    let refused = exchange(
        proxy.addr,
        b"GET /b HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(
        refused.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "{refused}"
    );
    assert!(
        refused.contains("x-steer-overflow: cluster_max_requests\r\n"),
        "{refused}"
    );
    assert!(refused.contains(
        "{\"error\":{\"code\":\"overloaded\",\"message\":\"too many concurrent requests to this service\""
    ));
    assert!(first.await.unwrap().ends_with("\r\n\r\nok"));
    // The first request's place is free again once its body is done.
    let again = exchange(
        proxy.addr,
        b"GET /c HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(again.starts_with("HTTP/1.1 200 OK\r\n"), "{again}");
}

#[tokio::test]
async fn cluster_timeout_before_the_head_is_504() {
    let (up, _) = upstream(OK, Duration::from_secs(3)).await;
    let proxy = start(&config(&[&up], ALL, r#","timeout":"200ms""#)).await;
    let began = Instant::now();
    let resp = exchange(
        proxy.addr,
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(began.elapsed() < Duration::from_secs(2));
    assert!(
        resp.starts_with("HTTP/1.1 504 Gateway Timeout\r\n"),
        "{resp}"
    );
    let id = request_id(&resp);
    assert_eq!(id.len(), 24);
    assert!(resp.ends_with(&format!(
        "{{\"error\":{{\"code\":\"upstream_timeout\",\"message\":\"the upstream did not answer in time\",\"request_id\":\"{id}\"}}}}\n"
    )));
}

#[tokio::test]
async fn cluster_timeout_cuts_a_streaming_body() {
    // The head and one chunk come at once; the rest never does.
    let reply = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nfirst\r\n";
    let (up, _) = upstream(reply, Duration::ZERO).await;
    let proxy = start(&config(&[&up], ALL, r#","timeout":"300ms""#)).await;
    let began = Instant::now();
    let resp = exchange(proxy.addr, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
    let took = began.elapsed();
    assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "{resp}");
    assert!(resp.contains("first"), "{resp}");
    // Cut, not finished: no last chunk.
    assert!(!resp.ends_with("0\r\n\r\n"), "{resp}");
    assert!(
        took >= Duration::from_millis(250) && took < Duration::from_secs(3),
        "{took:?}"
    );
}

#[tokio::test]
async fn unreachable_or_broken_upstreams_are_502() {
    // A port nobody listens on.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = closed.local_addr().unwrap().to_string();
    drop(closed);
    let (garbage, _) = upstream(b"NOT HTTP\r\n\r\n", Duration::ZERO).await;
    for up in [dead, garbage] {
        let proxy = start(&config(&[&up], ALL, "")).await;
        let resp = exchange(
            proxy.addr,
            b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 502 Bad Gateway\r\n"), "{resp}");
        assert!(resp.contains(
            "{\"error\":{\"code\":\"bad_gateway\",\"message\":\"the upstream failed to answer\""
        ));
    }
}

#[tokio::test]
async fn a_body_that_breaks_while_buffered_is_400() {
    let (up, mut seen) = upstream(OK, Duration::ZERO).await;
    let retry = r#","retry":{"max_retries":2}"#;
    let proxy = start(&config(&[&up], ALL, retry)).await;
    let mut conn = TcpStream::connect(proxy.addr).await.unwrap();
    conn.write_all(b"POST /p HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nabc")
        .await
        .unwrap();
    conn.shutdown().await.unwrap();
    let resp = read_all(&mut conn).await;
    assert!(resp.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{resp}");
    assert!(resp.contains(
        "{\"error\":{\"code\":\"bad_request\",\"message\":\"could not read the request body\""
    ));
    // The upstream was never contacted.
    assert!(seen.try_recv().is_err());
}

#[tokio::test]
async fn bodies_reach_the_upstream_whole() {
    let (up, mut seen) = upstream(OK, Duration::ZERO).await;
    let retry = r#","retry":{"max_retries":1,"max_body_bytes":8}"#;
    let proxy = start(&config(&[&up], ALL, retry)).await;
    // Buffered (4 bytes), and too long to buffer (12 bytes).
    for (request, body) in [
        (&b"POST /a HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd"[..], "abcd"),
        (b"POST /b HTTP/1.1\r\nHost: x\r\nContent-Length: 12\r\nConnection: close\r\n\r\nabcdefghijkl", "abcdefghijkl"),
    ] {
        let resp = exchange(proxy.addr, request).await;
        assert!(resp.ends_with("\r\n\r\nok"), "{resp}");
        let got = seen.recv().await.unwrap();
        assert!(got.ends_with(&format!("\r\n\r\n{body}")), "{got}");
        assert!(got.contains(&format!("content-length: {}\r\n", body.len())), "{got}");
    }
}

#[tokio::test]
async fn chunked_bodies_stay_chunked_past_the_buffer() {
    // An upstream that echoes what it read, raw.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up = listener.local_addr().unwrap().to_string();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        let mut got = Vec::new();
        let mut buf = [0; 4096];
        while !got.ends_with(b"0\r\n\r\n") {
            let n = conn.read(&mut buf).await.unwrap();
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        conn.write_all(OK).await.unwrap();
        let _ = tx.send(String::from_utf8(got).unwrap());
    });
    let retry = r#","retry":{"max_retries":1,"max_body_bytes":8}"#;
    let proxy = start(&config(&[&up], ALL, retry)).await;
    let resp = exchange(
        proxy.addr,
        b"POST /c HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n6\r\nabcdef\r\n6\r\nghijkl\r\n0\r\n\r\n",
    )
    .await;
    assert!(resp.ends_with("\r\n\r\nok"), "{resp}");
    let got = rx.recv().await.unwrap();
    assert!(got.contains("transfer-encoding: chunked\r\n"), "{got}");
    assert!(!got.contains("content-length"), "{got}");
    let body = &got[got.find("\r\n\r\n").unwrap() + 4..];
    let mut decoded = String::new();
    let mut rest = body;
    loop {
        let (size, after) = rest.split_once("\r\n").unwrap();
        let size = usize::from_str_radix(size, 16).unwrap();
        if size == 0 {
            break;
        }
        decoded.push_str(&after[..size]);
        rest = &after[size + 2..];
    }
    assert_eq!(decoded, "abcdefghijkl");
}

#[tokio::test]
async fn upstream_reason_phrases_become_the_standard_one() {
    let (up, _) = upstream(
        b"HTTP/1.1 200 Fine Thanks\r\nContent-Length: 0\r\n\r\n",
        Duration::ZERO,
    )
    .await;
    let proxy = start(&config(&[&up], ALL, "")).await;
    let resp = exchange(
        proxy.addr,
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "{resp}");
}

#[tokio::test]
async fn endpoints_of_the_preferred_priority_take_turns() {
    let (a, mut seen_a) = upstream(OK, Duration::ZERO).await;
    let (b, mut seen_b) = upstream(OK, Duration::ZERO).await;
    let (c, mut seen_c) = upstream(OK, Duration::ZERO).await;
    let cfg = format!(
        "{{\"listen\":\"127.0.0.1:0\",\"routes\":[{ALL}],\"clusters\":[{{\"name\":\"web\",\"endpoints\":[\
         {{\"address\":\"{c}\",\"priority\":1}},{{\"address\":\"{a}\",\"priority\":0}},{{\"address\":\"{b}\",\"priority\":0}}]}}]}}"
    );
    let proxy = start(&cfg).await;
    for _ in 0..4 {
        let resp = exchange(
            proxy.addr,
            b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(resp.ends_with("ok"), "{resp}");
    }
    for seen in [&mut seen_a, &mut seen_b] {
        seen.recv().await.unwrap();
        seen.recv().await.unwrap();
        assert!(seen.try_recv().is_err());
    }
    assert!(seen_c.try_recv().is_err());
}

#[tokio::test]
async fn absolute_form_targets_route_on_their_authority() {
    let (up, mut seen) = upstream(OK, Duration::ZERO).await;
    let routes = r#"{"host":"api.example.com","path_prefix":"/","cluster":"web"}"#;
    let proxy = start(&config(&[&up], routes, "")).await;
    let resp = exchange(
        proxy.addr,
        b"GET http://api.example.com/v1?q=1 HTTP/1.1\r\nHost: other.example.com\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "{resp}");
    let got = seen.recv().await.unwrap();
    assert!(got.starts_with("GET /v1?q=1 HTTP/1.1\r\n"), "{got}");
    assert!(
        got.contains("x-forwarded-host: api.example.com\r\n"),
        "{got}"
    );
    assert!(got.contains("host: api.example.com\r\n"), "{got}");
    assert!(!got.contains("other.example.com"), "{got}");
}

#[tokio::test]
async fn requests_gos_server_refuses_never_reach_the_upstream() {
    let (up, mut seen) = upstream(OK, Duration::ZERO).await;
    let proxy = start(&config(&[&up], ALL, "")).await;
    let plain = "content-type: text/plain; charset=utf-8\r\n";
    for (request, status, body) in [
        (
            &b"GET /bad%zz HTTP/1.1\r\nHost: x\r\n\r\n"[..],
            "400 Bad Request",
            "400 Bad Request",
        ),
        (
            b"GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n",
            "400 Bad Request",
            "400 Bad Request",
        ),
        (
            b"GET / HTTP/1.1\r\n\r\n",
            "400 Bad Request: missing required Host header",
            "400 Bad Request: missing required Host header",
        ),
        (
            b"GET / HTTP/1.1\r\nHost: a b\r\n\r\n",
            "400 Bad Request: malformed Host header",
            "400 Bad Request: malformed Host header",
        ),
    ] {
        let resp = exchange(proxy.addr, request).await;
        assert!(
            resp.starts_with(&format!("HTTP/1.1 {status}\r\n")),
            "{resp}"
        );
        assert!(
            resp.contains(plain) && resp.contains("connection: close\r\n"),
            "{resp}"
        );
        assert!(resp.ends_with(&format!("\r\n\r\n{body}")), "{resp}");
    }

    let resp = exchange(
        proxy.addr,
        b"OPTIONS * HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "{resp}");
    assert!(resp.contains("content-length: 0\r\n"), "{resp}");

    let resp = exchange(proxy.addr, b"CONNECT x:443 HTTP/1.1\r\nHost: x:443\r\n\r\n").await;
    assert!(
        resp.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"),
        "{resp}"
    );
    assert!(
        resp.contains("allow: GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS\r\n"),
        "{resp}"
    );
    assert!(resp.contains("connection: close\r\n"), "{resp}");
    assert!(
        resp.contains("\"code\":\"method_not_allowed\",\"message\":\"CONNECT is not supported\"")
    );

    // HTTP/1.0 may leave Host out; the upstream then gets the endpoint's.
    let resp = exchange(proxy.addr, b"GET /old HTTP/1.0\r\n\r\n").await;
    assert!(resp.ends_with("\r\n\r\nok"), "{resp}");
    let got = seen.recv().await.unwrap();
    assert!(got.contains(&format!("host: {up}\r\n")), "{got}");
    assert!(!got.contains("x-forwarded-host"), "{got}");
    assert!(seen.try_recv().is_err());
}
