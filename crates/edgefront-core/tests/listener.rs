//! The listener's limits, keep-alive timers and graceful drain, exercised
//! over real TCP connections with hand-written HTTP/1.1.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use edgefront_core::listener::{Handler, ServeReport, ServerLimits, serve};
use http::{Request, Response};
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Sleep;

/// A body that is either complete or sends `left` chunks, one per `every`.
enum TestBody {
    Full(Full<Bytes>),
    Drip {
        left: usize,
        every: Duration,
        sleep: Option<Pin<Box<Sleep>>>,
    },
}

impl Body for TestBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match self.get_mut() {
            TestBody::Full(full) => Pin::new(full).poll_frame(cx),
            TestBody::Drip { left, every, sleep } => {
                if *left == 0 {
                    return Poll::Ready(None);
                }
                let timer = sleep.get_or_insert_with(|| Box::pin(tokio::time::sleep(*every)));
                if timer.as_mut().poll(cx).is_pending() {
                    return Poll::Pending;
                }
                *sleep = None;
                *left -= 1;
                Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"drip\n")))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            TestBody::Full(full) => full.is_end_stream(),
            TestBody::Drip { left, .. } => *left == 0,
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            TestBody::Full(full) => full.size_hint(),
            TestBody::Drip { .. } => SizeHint::default(),
        }
    }
}

/// `/` answers at once, `/slow/<ms>` after a delay, `/drip/<n>/<ms>`
/// streams n chunks, `/echo` returns the request body, `/hang` never
/// answers.
struct App;

impl Handler for App {
    type Body = TestBody;

    async fn call(self: Arc<Self>, req: Request<Incoming>, peer: SocketAddr) -> Response<TestBody> {
        let path = req.uri().path().to_owned();
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match parts.as_slice() {
            ["slow", ms] => {
                tokio::time::sleep(Duration::from_millis(ms.parse().unwrap())).await;
            }
            ["drip", n, ms] => {
                return Response::new(TestBody::Drip {
                    left: n.parse().unwrap(),
                    every: Duration::from_millis(ms.parse().unwrap()),
                    sleep: None,
                });
            }
            ["hang"] => std::future::pending::<()>().await,
            ["echo"] => {
                let body = req.into_body().collect().await.unwrap().to_bytes();
                return Response::new(TestBody::Full(Full::new(body)));
            }
            _ => {}
        }
        let body = format!("ok {}", peer.ip());
        Response::new(TestBody::Full(Full::new(Bytes::from(body))))
    }
}

struct Server {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<ServeReport>,
}

impl Server {
    async fn start(limits: ServerLimits) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(serve(listener, Arc::new(App), limits, async {
            let _ = rx.await;
        }));
        Server {
            addr,
            stop: Some(tx),
            task,
        }
    }

    async fn connect(&self) -> TcpStream {
        TcpStream::connect(self.addr).await.unwrap()
    }

    fn begin_shutdown(&mut self) {
        let _ = self.stop.take().unwrap().send(());
    }

    async fn stop(mut self) -> ServeReport {
        self.begin_shutdown();
        self.task.await.unwrap()
    }
}

fn limits() -> ServerLimits {
    ServerLimits {
        header_read_timeout: Duration::from_millis(300),
        idle_timeout: Duration::from_millis(900),
        max_header_bytes: 4096,
        max_headers: 32,
        drain_timeout: Duration::from_secs(5),
    }
}

fn get(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nhost: test\r\n\r\n")
}

/// Reads one response: the head, then a Content-Length or chunked body.
async fn read_response(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        let n = s.read(&mut byte).await.unwrap();
        assert!(
            n > 0,
            "closed mid-head: {:?}",
            String::from_utf8_lossy(&buf)
        );
        buf.push(byte[0]);
    }
    let head = String::from_utf8(buf.clone()).unwrap().to_ascii_lowercase();
    if let Some(len) = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length: "))
    {
        let mut body = vec![0u8; len.trim().parse().unwrap()];
        s.read_exact(&mut body).await.unwrap();
        buf.extend_from_slice(&body);
    } else if head.contains("transfer-encoding: chunked") {
        while !buf.ends_with(b"\r\n0\r\n\r\n") {
            let n = s.read(&mut byte).await.unwrap();
            assert!(n > 0, "closed mid-body");
            buf.push(byte[0]);
        }
    }
    String::from_utf8(buf).unwrap()
}

async fn round_trip(s: &mut TcpStream, path: &str) -> String {
    s.write_all(get(path).as_bytes()).await.unwrap();
    read_response(s).await
}

/// Waits for the server to close the connection; returns how long it took.
async fn closed_after(s: &mut TcpStream, from: Instant) -> Duration {
    let mut buf = [0u8; 256];
    match tokio::time::timeout(Duration::from_secs(10), s.read(&mut buf)).await {
        Ok(Ok(0)) | Ok(Err(_)) => from.elapsed(),
        Ok(Ok(n)) => panic!("unexpected bytes: {:?}", String::from_utf8_lossy(&buf[..n])),
        Err(_) => panic!("connection still open after 10 s"),
    }
}

fn status(resp: &str) -> &str {
    resp.split(' ').nth(1).unwrap()
}

/// A request head of exactly `len` bytes.
fn head_of_len(len: usize) -> String {
    let bare = "GET / HTTP/1.1\r\nhost: test\r\nx-pad: \r\n\r\n".len();
    let pad = "a".repeat(len - bare);
    let head = format!("GET / HTTP/1.1\r\nhost: test\r\nx-pad: {pad}\r\n\r\n");
    assert_eq!(head.len(), len);
    head
}

#[tokio::test]
async fn serves_several_requests_on_one_connection() {
    let server = Server::start(limits()).await;
    let mut s = server.connect().await;
    for _ in 0..3 {
        let resp = round_trip(&mut s, "/").await;
        assert_eq!(status(&resp), "200");
        assert!(resp.ends_with("ok 127.0.0.1"), "{resp}");
    }
    let report = server.stop().await;
    assert_eq!(
        report,
        ServeReport {
            accepted: 1,
            forced: 0
        }
    );
}

async fn check_head_limit(max: usize) {
    let server = Server::start(ServerLimits {
        max_header_bytes: max,
        ..limits()
    })
    .await;
    let mut s = server.connect().await;
    s.write_all(head_of_len(max).as_bytes()).await.unwrap();
    assert_eq!(status(&read_response(&mut s).await), "200");

    let mut s = server.connect().await;
    s.write_all(head_of_len(max + 1).as_bytes()).await.unwrap();
    assert_eq!(status(&read_response(&mut s).await), "431");
    closed_after(&mut s, Instant::now()).await;
}

#[tokio::test]
async fn head_size_limit_is_exact_below_the_read_buffer() {
    check_head_limit(200).await;
}

#[tokio::test]
async fn head_size_limit_is_exact_above_the_read_buffer() {
    check_head_limit(20_000).await;
}

#[tokio::test]
async fn too_many_header_fields_get_431() {
    let server = Server::start(ServerLimits {
        max_headers: 4,
        ..limits()
    })
    .await;
    let fields = |n: usize| -> String {
        let mut head = String::from("GET / HTTP/1.1\r\nhost: test\r\n");
        for i in 1..n {
            head.push_str(&format!("x-{i}: v\r\n"));
        }
        head + "\r\n"
    };
    let mut s = server.connect().await;
    s.write_all(fields(4).as_bytes()).await.unwrap();
    assert_eq!(status(&read_response(&mut s).await), "200");
    let mut s = server.connect().await;
    s.write_all(fields(5).as_bytes()).await.unwrap();
    assert_eq!(status(&read_response(&mut s).await), "431");
}

#[tokio::test]
async fn silent_connection_is_closed_after_the_header_timeout() {
    let server = Server::start(limits()).await;
    let start = Instant::now();
    let mut s = server.connect().await;
    let took = closed_after(&mut s, start).await;
    // The first head's timer runs from the accept, not the idle timeout.
    assert!(took >= Duration::from_millis(280), "{took:?}");
    assert!(took < Duration::from_millis(800), "{took:?}");
}

#[tokio::test]
async fn stalled_head_is_closed_after_the_header_timeout() {
    let server = Server::start(limits()).await;
    let mut s = server.connect().await;
    s.write_all(b"GET / HTTP/1.1\r\nhost: te").await.unwrap();
    let start = Instant::now();
    let took = closed_after(&mut s, start).await;
    assert!(took < Duration::from_millis(800), "{took:?}");

    // The same after a completed request: the timer starts at the first
    // byte of the next head.
    let mut s = server.connect().await;
    round_trip(&mut s, "/").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    s.write_all(b"GET / HT").await.unwrap();
    let start = Instant::now();
    let took = closed_after(&mut s, start).await;
    assert!(took >= Duration::from_millis(250), "{took:?}");
    assert!(took < Duration::from_millis(800), "{took:?}");
}

#[tokio::test]
async fn idle_keep_alive_connection_is_closed_after_the_idle_timeout() {
    let server = Server::start(limits()).await;
    let mut s = server.connect().await;
    round_trip(&mut s, "/").await;
    let start = Instant::now();
    let took = closed_after(&mut s, start).await;
    // Longer than the header timeout: idle connections get their own.
    assert!(took >= Duration::from_millis(850), "{took:?}");
    assert!(took < Duration::from_millis(2000), "{took:?}");
}

#[tokio::test]
async fn a_head_started_while_idle_is_not_cut_by_the_idle_timer() {
    let server = Server::start(ServerLimits {
        header_read_timeout: Duration::from_millis(600),
        idle_timeout: Duration::from_millis(500),
        ..limits()
    })
    .await;
    let mut s = server.connect().await;
    round_trip(&mut s, "/").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
    // 700 ms since the response: past the idle timeout, inside the head's.
    tokio::time::sleep(Duration::from_millis(400)).await;
    s.write_all(b"host: test\r\n\r\n").await.unwrap();
    assert_eq!(status(&read_response(&mut s).await), "200");
}

#[tokio::test]
async fn a_slow_handler_outlives_both_timers() {
    let server = Server::start(limits()).await;
    let mut s = server.connect().await;
    let resp = round_trip(&mut s, "/slow/1300").await;
    assert_eq!(status(&resp), "200");
    // The connection is reusable afterwards.
    assert_eq!(status(&round_trip(&mut s, "/").await), "200");
}

#[tokio::test]
async fn a_streamed_response_outlives_the_idle_timer() {
    let server = Server::start(limits()).await;
    let mut s = server.connect().await;
    let resp = round_trip(&mut s, "/drip/6/250").await;
    assert_eq!(status(&resp), "200");
    assert_eq!(resp.matches("drip").count(), 6, "{resp}");
}

#[tokio::test]
async fn a_slow_upload_outlives_the_header_timer() {
    let server = Server::start(limits()).await;
    let mut s = server.connect().await;
    s.write_all(b"POST /echo HTTP/1.1\r\nhost: test\r\ncontent-length: 4\r\n\r\n")
        .await
        .unwrap();
    // 800 ms of body after the head: well past the header timeout, but
    // the request is in flight while its body arrives.
    for b in b"abcd" {
        tokio::time::sleep(Duration::from_millis(200)).await;
        s.write_all(&[*b]).await.unwrap();
    }
    let resp = read_response(&mut s).await;
    assert_eq!(status(&resp), "200");
    assert!(resp.ends_with("\r\n\r\nabcd"), "{resp}");
}

#[tokio::test]
async fn shutdown_closes_idle_connections_and_finishes_in_flight_ones() {
    let mut server = Server::start(ServerLimits {
        idle_timeout: Duration::from_secs(30),
        ..limits()
    })
    .await;
    let mut idle = server.connect().await;
    round_trip(&mut idle, "/").await;
    let mut busy = server.connect().await;
    busy.write_all(get("/slow/600").as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    server.begin_shutdown();
    let start = Instant::now();
    let took = closed_after(&mut idle, start).await;
    assert!(took < Duration::from_millis(300), "{took:?}");

    let resp = read_response(&mut busy).await;
    assert_eq!(status(&resp), "200");
    assert!(
        resp.to_ascii_lowercase().contains("connection: close"),
        "{resp}"
    );
    closed_after(&mut busy, Instant::now()).await;

    let report = server.task.await.unwrap();
    assert_eq!(report.accepted, 2);
    assert_eq!(report.forced, 0);
    assert!(TcpStream::connect(server.addr).await.is_err());
}

#[tokio::test]
async fn drain_timeout_closes_requests_that_never_finish() {
    let mut server = Server::start(ServerLimits {
        drain_timeout: Duration::from_millis(300),
        ..limits()
    })
    .await;
    let mut hung = server.connect().await;
    hung.write_all(get("/hang").as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let start = Instant::now();
    server.begin_shutdown();
    let report = server.task.await.unwrap();
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(280), "{took:?}");
    assert!(took < Duration::from_secs(2), "{took:?}");
    assert_eq!(report.forced, 1);
    closed_after(&mut hung, start).await;
}
