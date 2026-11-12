//! The fake upstream, steer's `internal/fakeupstream` tests translated:
//! settings patches, injected faults, the blackhole and the control API,
//! with the response bodies byte for byte as steer wrote them.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use edgefront_core::http1::{Client, ClientError, ClientLimits};
use edgefront_core::listener::{ServerLimits, serve};
use edgefront_proxy::fakeupstream::{
    Control, MAX_CONTROL_BODY, MAX_LATENCY, Server, Settings, parse_settings,
};
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use proptest::prelude::*;
use tokio::net::TcpListener;

#[test]
fn settings_patches() {
    let cur = Settings {
        latency: Duration::from_secs(1),
        ..Settings::default()
    };
    let got = parse_settings(cur, br#"{"error_rate":0.5,"blackhole":true}"#).unwrap();
    assert_eq!(
        got,
        Settings {
            latency: Duration::from_secs(1),
            error_rate: 0.5,
            blackhole: true
        }
    );
    let got = parse_settings(got, br#"{"latency":"0s","blackhole":false}"#).unwrap();
    assert_eq!(
        got,
        Settings {
            latency: Duration::ZERO,
            error_rate: 0.5,
            blackhole: false
        }
    );
    let too_big = " ".repeat(MAX_CONTROL_BODY + 1);
    for bad in [
        "nope",
        "",
        "[]",
        r#"{"x":1}"#,
        "{} {}",
        r#"{"latency":"-1s"}"#,
        r#"{"latency":"2m"}"#,
        r#"{"latency":"soon"}"#,
        r#"{"latency":5}"#,
        r#"{"error_rate":1.5}"#,
        r#"{"error_rate":-0.1}"#,
        r#"{"error_rate":"0.5"}"#,
        r#"{"blackhole":"yes"}"#,
        &too_big,
    ] {
        assert!(
            parse_settings(cur, bad.as_bytes()).is_err(),
            "accepted {bad:?}"
        );
    }
    assert_eq!(
        parse_settings(cur, b"{} {}").unwrap_err(),
        "body must be a single JSON object"
    );
    assert_eq!(
        parse_settings(cur, br#"{"latency":"2m"}"#).unwrap_err(),
        "latency must be a duration between 0s and 1m0s"
    );
    assert!(
        parse_settings(cur, br#"{"x":1}"#)
            .unwrap_err()
            .contains("unknown field \"x\"")
    );
}

/// What Go's `encoding/json` does with a struct of optional fields.
#[test]
fn settings_patches_decode_like_go() {
    let cur = Settings {
        latency: Duration::from_secs(1),
        error_rate: 0.5,
        blackhole: true,
    };
    // A top-level null is an empty patch, and so is an empty object.
    assert_eq!(parse_settings(cur, b"null"), Ok(cur));
    assert_eq!(parse_settings(cur, b" {} \n"), Ok(cur));
    // Field names match without regard to case, Kelvin sign included.
    let got = parse_settings(
        cur,
        "{\"Error_Rate\":0.25,\"BLAC\u{212a}HOLE\":false}".as_bytes(),
    );
    assert_eq!(got.map(|s| (s.error_rate, s.blackhole)), Ok((0.25, false)));
    // A later duplicate wins, in document order.
    let got = parse_settings(cur, br#"{"latency":"1ms","Latency":"2ms"}"#).unwrap();
    assert_eq!(got.latency, Duration::from_millis(2));
    // null leaves a field as it was.
    assert_eq!(
        parse_settings(
            cur,
            br#"{"latency":null,"error_rate":null,"blackhole":null}"#
        ),
        Ok(cur)
    );
    // An empty latency string is no change.
    assert_eq!(parse_settings(cur, br#"{"latency":""}"#), Ok(cur));
    assert_eq!(
        parse_settings(cur, br#"{"latency":"1m0s","error_rate":0}"#).map(|s| s.latency),
        Ok(MAX_LATENCY)
    );
}

proptest! {
    /// steer's FuzzParseSettings: whatever is accepted is in range.
    #[test]
    fn accepted_settings_are_in_range(data in proptest::collection::vec(any::<u8>(), 0..64),
                                      latency in "-?[0-9]{1,3}(\\.[0-9])?(ns|us|ms|s|m|h)",
                                      rate in -2.0f64..2.0, hole in any::<bool>()) {
        let doc = format!(r#"{{"latency":"{latency}","error_rate":{rate},"blackhole":{hole}}}"#);
        for input in [data, doc.into_bytes()] {
            if let Ok(s) = parse_settings(Settings::default(), &input) {
                prop_assert!(s.latency <= MAX_LATENCY);
                prop_assert!((0.0..=1.0).contains(&s.error_rate));
            }
        }
    }
}

fn get(path: &str) -> Request<Full<Bytes>> {
    Request::builder().uri(path).body(Full::default()).unwrap()
}

async fn body_of(resp: Response<Full<Bytes>>) -> String {
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(b.to_vec()).unwrap()
}

#[tokio::test]
async fn faults() {
    let s = Server::with_seed("a", 7);
    assert_eq!(s.name(), "a");
    let resp = s.handle(get("/x?q=1")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-upstream"], "a");
    assert_eq!(resp.headers()["content-type"], "application/json");
    assert_eq!(
        body_of(resp).await,
        "{\"method\":\"GET\",\"path\":\"/x\",\"upstream\":\"a\"}\n"
    );
    // The decoded path, escaped as Go escapes it.
    let resp = s.handle(get("/a%3Cb%3E%26")).await;
    assert_eq!(
        body_of(resp).await,
        "{\"method\":\"GET\",\"path\":\"/a\\u003cb\\u003e\\u0026\",\"upstream\":\"a\"}\n"
    );
    assert_eq!(s.handle(get("/bad%zz")).await.status(), 400);

    s.set(Settings {
        error_rate: 1.0,
        ..Settings::default()
    });
    let resp = s.handle(get("/x")).await;
    assert_eq!(resp.status(), 503);
    assert_eq!(
        body_of(resp).await,
        "{\"error\":\"injected failure\",\"upstream\":\"a\"}\n"
    );
    let resp = s.handle(get("/healthz")).await;
    assert_eq!(
        resp.status(),
        200,
        "the health check must ignore the error rate"
    );
    assert_eq!(body_of(resp).await, "{\"status\":\"ok\"}\n");

    s.set(Settings {
        latency: Duration::from_millis(30),
        ..Settings::default()
    });
    let started = Instant::now();
    s.handle(get("/x")).await;
    assert!(
        started.elapsed() >= Duration::from_millis(30),
        "latency not applied"
    );
    // Latency gives up when the client does.
    s.set(Settings {
        latency: Duration::from_secs(60),
        ..Settings::default()
    });
    let started = Instant::now();
    assert!(
        tokio::time::timeout(Duration::from_millis(10), s.handle(get("/x")))
            .await
            .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(s.counts(), (3, 1, 0));
    assert!(format!("{s:?}").contains("served: 3"));
}

/// About half the requests fail at an error rate of one half.
#[tokio::test]
async fn error_rate_is_a_fraction() {
    let s = Server::with_seed("r", 42);
    s.set(Settings {
        error_rate: 0.5,
        ..Settings::default()
    });
    for _ in 0..2000 {
        s.handle(get("/")).await;
    }
    let (served, failed, _) = s.counts();
    assert_eq!(served + failed, 2000);
    assert!((900..1100).contains(&failed), "failed {failed}");
}

async fn start_on(s: Arc<Server>) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let limits = ServerLimits {
        drain_timeout: Duration::from_millis(100),
        ..ServerLimits::default()
    };
    tokio::spawn(serve(listener, s, limits, async move {
        let _ = stopped.await;
    }));
    (addr, stop)
}

#[tokio::test]
async fn blackhole_holds_until_the_client_gives_up() {
    let s = Arc::new(Server::new("b"));
    let (addr, _stop) = start_on(Arc::clone(&s)).await;
    s.set(Settings {
        blackhole: true,
        ..Settings::default()
    });
    let client = Client::<Full<Bytes>>::new(&ClientLimits {
        response_header_timeout: Some(Duration::from_millis(100)),
        ..ClientLimits::default()
    });
    let started = Instant::now();
    let got = client.send(get(&format!("http://{addr}/healthz"))).await;
    assert_eq!(
        got.err(),
        Some(ClientError::Timeout),
        "the blackhole answered"
    );
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert_eq!(s.counts().2, 1);

    // Close releases held requests, with an empty answer.
    let held = tokio::spawn({
        let s = Arc::clone(&s);
        async move { s.handle(get("/")).await }
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    s.close();
    let resp = tokio::time::timeout(Duration::from_secs(5), held)
        .await
        .expect("close did not release the blackhole")
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(body_of(resp).await, "");
    // And from then on the blackhole holds nothing.
    let resp = tokio::time::timeout(Duration::from_secs(5), s.handle(get("/")))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// A request body is read, up to a limit, before the answer.
#[tokio::test]
async fn request_bodies_are_read() {
    let s = Arc::new(Server::new("p"));
    let (addr, _stop) = start_on(Arc::clone(&s)).await;
    let client = Client::<Full<Bytes>>::new(&ClientLimits::default());
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{addr}/upload"))
        .body(Full::new(Bytes::from(vec![b'z'; 1 << 20])))
        .unwrap();
    let resp = client.send(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        &b[..],
        b"{\"method\":\"POST\",\"path\":\"/upload\",\"upstream\":\"p\"}\n"
    );
}

#[tokio::test]
async fn control() {
    let s = Arc::new(Server::new("c"));
    let c = Control(Arc::clone(&s));
    let call = |method: Method, path: &str, body: Vec<u8>| {
        let c = c.clone();
        let req = Request::builder()
            .method(method)
            .uri(path)
            .body(Full::new(Bytes::from(body)))
            .unwrap();
        async move {
            let resp = c.handle(req).await;
            let status = resp.status();
            let allow = resp.headers().get("allow").cloned();
            (status, allow, body_of(resp).await)
        }
    };
    let (status, _, body) = call(
        Method::PUT,
        "/state",
        br#"{"error_rate":0.5,"latency":"5ms"}"#.to_vec(),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        "{\"failed\":0,\"held\":0,\"name\":\"c\",\"served\":0,\"settings\":{\"blackhole\":false,\"error_rate\":0.5,\"latency\":\"5ms\"}}\n"
    );
    s.handle(get("/healthz")).await;
    let (status, _, body) = call(Method::GET, "/state", Vec::new()).await;
    assert_eq!(status, 200);
    assert!(
        body.starts_with("{\"failed\":0,\"held\":0,\"name\":\"c\""),
        "{body}"
    );

    let (status, _, body) = call(Method::PUT, "/state", br#"{"error_rate":2}"#.to_vec()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        "{\"error\":{\"code\":\"invalid_argument\",\"message\":\"error_rate must be between 0 and 1\"}}\n"
    );
    assert_eq!(
        s.settings().error_rate,
        0.5,
        "a rejected patch changed the settings"
    );

    let (status, allow, body) = call(Method::DELETE, "/state", Vec::new()).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(allow.unwrap(), "GET, PUT");
    assert!(body.contains("method_not_allowed"));

    let (status, _, _) = call(Method::PUT, "/state", vec![b'x'; MAX_CONTROL_BODY + 10]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "oversized");

    let (status, _, body) = call(Method::GET, "/other", Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, "404 page not found\n");
}

/// The control API over TCP, as the failover drill drives it.
#[tokio::test]
async fn control_over_tcp() {
    let s = Arc::new(Server::new("t"));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (_stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve(
        listener,
        Arc::new(Control(Arc::clone(&s))),
        ServerLimits::default(),
        async move {
            let _ = stopped.await;
        },
    ));
    let client = Client::<Full<Bytes>>::new(&ClientLimits::default());
    let req = Request::builder()
        .method(Method::PUT)
        .uri(format!("http://{addr}/state"))
        .body(Full::new(Bytes::from_static(br#"{"blackhole":true}"#)))
        .unwrap();
    assert_eq!(client.send(req).await.unwrap().status(), 200);
    assert!(s.settings().blackhole);
}
