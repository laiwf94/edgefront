//! The upstream HTTP/2 pool against real servers over loopback: streams
//! multiplexed on one connection, the connection cap at 10,000 streams,
//! GOAWAY in the middle of requests (graceful, and refusing a stream the
//! origin never processed), idle reaping, and TLS with ALPN.

#[path = "common/tls.rs"]
mod common;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use common::*;
use edgefront_core::config::{Http2, UpstreamTls};
use edgefront_core::h2::{Pool, PoolError, PoolLimits, Settings};
use edgefront_core::http1::TlsTo;
use edgefront_core::listener::{Handler, ServerLimits, serve, serve_tls};
use edgefront_core::tls::{Acceptor, Shared, Snapshot, Store};
use http::{Request, Response, Version};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// Counts connections (by peer) and the most requests in flight at once,
/// holding each request for `hold`.
struct Counting {
    hold: Duration,
    peers: Mutex<HashSet<SocketAddr>>,
    now: AtomicUsize,
    peak: AtomicUsize,
    /// Requests wait until this many are in flight at once (0: no wait).
    gate: usize,
    open: tokio::sync::Notify,
}

impl Counting {
    fn new(hold: Duration) -> Arc<Counting> {
        Counting::gated(hold, 0)
    }

    fn gated(hold: Duration, gate: usize) -> Arc<Counting> {
        Arc::new(Counting {
            hold,
            peers: Mutex::new(HashSet::new()),
            now: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            gate,
            open: tokio::sync::Notify::new(),
        })
    }

    fn connections(&self) -> usize {
        self.peers.lock().unwrap().len()
    }
}

impl Handler for Counting {
    type Body = Full<Bytes>;

    async fn call(
        self: Arc<Self>,
        req: Request<Incoming>,
        peer: SocketAddr,
    ) -> Response<Full<Bytes>> {
        self.peers.lock().unwrap().insert(peer);
        let n = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(n, Ordering::SeqCst);
        if self.gate > 0 {
            let opened = self.open.notified();
            if self.peak.load(Ordering::SeqCst) >= self.gate {
                self.open.notify_waiters();
            } else {
                opened.await;
            }
        }
        tokio::time::sleep(self.hold).await;
        self.now.fetch_sub(1, Ordering::SeqCst);
        let text = format!("{:?} {}", req.version(), req.uri().path());
        Response::new(Full::new(Bytes::from(text)))
    }
}

fn server_settings(streams: u32) -> Settings {
    Settings::from(&Http2 {
        max_concurrent_streams: streams,
        cleartext: true,
        ..Http2::default()
    })
}

async fn h2c_server(handler: Arc<Counting>, streams: u32) -> (SocketAddr, oneshot::Sender<()>) {
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let limits = ServerLimits {
        http2: Some(server_settings(streams)),
        drain_timeout: Duration::from_secs(2),
        ..ServerLimits::default()
    };
    tokio::spawn(serve(socket, handler, limits, async move {
        let _ = stopped.await;
    }));
    (addr, stop)
}

fn get(addr: SocketAddr, path: &str) -> Request<Full<Bytes>> {
    Request::get(format!("http://{addr}{path}"))
        .body(Full::new(Bytes::new()))
        .unwrap()
}

async fn text(resp: Response<edgefront_core::h2::PoolBody>) -> String {
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&body).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_connection_carries_100_concurrent_streams() {
    let handler = Counting::new(Duration::from_millis(200));
    let (addr, _stop) = h2c_server(Arc::clone(&handler), 1000).await;
    let pool: Pool<Full<Bytes>> = Pool::new(PoolLimits::default(), Settings::default());
    let mut tasks = Vec::new();
    for i in 0..100 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            let resp = pool.send(get(addr, &format!("/{i}"))).await.unwrap();
            assert_eq!(resp.version(), Version::HTTP_2);
            text(resp).await
        }));
    }
    for (i, t) in tasks.into_iter().enumerate() {
        assert_eq!(t.await.unwrap(), format!("HTTP/2.0 /{i}"));
    }
    assert_eq!(handler.connections(), 1, "one upstream connection");
    assert_eq!(handler.peak.load(Ordering::SeqCst), 100, "all 100 at once");
    assert_eq!(pool.opened(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ten_thousand_streams_use_at_most_four_connections() {
    // Every request waits until 1,000 are in flight at once, well past the
    // 400 that four connections of 100 streams hold before overflowing.
    let handler = Counting::gated(Duration::from_millis(1), 1000);
    let (addr, _stop) = h2c_server(Arc::clone(&handler), 1000).await;
    let pool: Pool<Full<Bytes>> = Pool::new(PoolLimits::default(), Settings::default());
    let mut tasks = Vec::new();
    for i in 0..10_000 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            pool.send(get(addr, &format!("/{i}")))
                .await
                .map(|r| r.status().as_u16())
        }));
    }
    let mut ok = 0;
    for t in tasks {
        if t.await.unwrap() == Ok(200) {
            ok += 1;
        }
    }
    assert_eq!(ok, 10_000);
    let conns = handler.connections();
    eprintln!(
        "10,000 streams: {conns} connections, at most {} requests at once",
        handler.peak.load(Ordering::SeqCst)
    );
    assert!((1..=4).contains(&conns), "{conns} connections");
    assert!(pool.opened() <= 4, "{} opened", pool.opened());
    // The cap is per origin and streams overflowed onto open connections.
    assert!(handler.peak.load(Ordering::SeqCst) >= 1000);
}

#[tokio::test]
async fn the_stream_cap_opens_more_connections_up_to_the_limit() {
    let handler = Counting::new(Duration::from_millis(200));
    let (addr, _stop) = h2c_server(Arc::clone(&handler), 1000).await;
    let limits = PoolLimits {
        max_connections: 3,
        max_streams_per_connection: 10,
        ..PoolLimits::default()
    };
    let pool: Pool<Full<Bytes>> = Pool::new(limits, Settings::default());
    let mut tasks = Vec::new();
    for i in 0..25 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            pool.send(get(addr, &format!("/{i}")))
                .await
                .unwrap()
                .status()
        }));
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    assert_eq!(handler.connections(), 3);
    assert_eq!(pool.connections(&addr.to_string()), 3);
}

#[tokio::test]
async fn idle_connections_are_reaped() {
    let handler = Counting::new(Duration::ZERO);
    let (addr, _stop) = h2c_server(Arc::clone(&handler), 100).await;
    let limits = PoolLimits {
        idle_timeout: Duration::from_millis(200),
        ..PoolLimits::default()
    };
    let pool: Pool<Full<Bytes>> = Pool::new(limits, Settings::default());
    let resp = pool.send(get(addr, "/a")).await.unwrap();
    // A stream still being read keeps its connection.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(pool.connections(&addr.to_string()), 1);
    assert_eq!(text(resp).await, "HTTP/2.0 /a");
    let mut gone = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if pool.connections(&addr.to_string()) == 0 {
            gone = true;
            break;
        }
    }
    assert!(gone, "the idle connection was closed");
    // The next request opens a new one.
    assert_eq!(pool.send(get(addr, "/b")).await.unwrap().status(), 200);
    assert_eq!(pool.opened(), 2);
}

/// A raw HTTP/2 origin. On its first connection it waits until two
/// requests have arrived, sends `GOAWAY` with the first one as the last
/// stream it will process, and answers that one; on later connections it
/// answers every request at once. Responses are a bare `:status: 200`.
async fn goaway_origin() -> (SocketAddr, Arc<AtomicUsize>) {
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let conns = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&conns);
    tokio::spawn(async move {
        loop {
            let (tcp, _) = socket.accept().await.unwrap();
            let n = counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(raw_conn(tcp, n == 0));
        }
    });
    (addr, conns)
}

async fn write_frame(tcp: &mut TcpStream, kind: u8, flags: u8, stream: u32, payload: &[u8]) {
    let len = payload.len() as u32;
    let mut f = vec![(len >> 16) as u8, (len >> 8) as u8, len as u8, kind, flags];
    f.extend_from_slice(&stream.to_be_bytes());
    f.extend_from_slice(payload);
    tcp.write_all(&f).await.unwrap();
}

async fn raw_conn(mut tcp: TcpStream, goaway_first: bool) {
    let mut preface = [0u8; 24];
    if tcp.read_exact(&mut preface).await.is_err() {
        return;
    }
    write_frame(&mut tcp, 0x4, 0, 0, &[]).await;
    let mut streams = Vec::new();
    loop {
        let mut head = [0u8; 9];
        if tcp.read_exact(&mut head).await.is_err() {
            return;
        }
        let len = u32::from_be_bytes([0, head[0], head[1], head[2]]) as usize;
        let mut payload = vec![0u8; len];
        if tcp.read_exact(&mut payload).await.is_err() {
            return;
        }
        let (kind, flags) = (head[3], head[4]);
        let stream = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff;
        match kind {
            0x4 if flags & 1 == 0 => write_frame(&mut tcp, 0x4, 1, 0, &[]).await,
            0x1 => {
                streams.push(stream);
                if !goaway_first {
                    // :status 200 (static table index 8), END_STREAM | END_HEADERS.
                    write_frame(&mut tcp, 0x1, 0x5, stream, &[0x88]).await;
                } else if streams.len() == 2 {
                    let mut goaway = streams[0].to_be_bytes().to_vec();
                    goaway.extend_from_slice(&0u32.to_be_bytes());
                    write_frame(&mut tcp, 0x7, 0, 0, &goaway).await;
                    write_frame(&mut tcp, 0x1, 0x5, streams[0], &[0x88]).await;
                }
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn goaway_mid_request_refuses_the_unprocessed_stream_and_it_is_sent_again() {
    let (addr, conns) = goaway_origin().await;
    let pool: Pool<Full<Bytes>> = Pool::new(PoolLimits::default(), Settings::default());
    let first = {
        let pool = pool.clone();
        tokio::spawn(async move { pool.send(get(addr, "/first")).await.map(|r| r.status()) })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The second stream goes on the same connection, and the origin's
    // GOAWAY excludes it: refused, then sent again on a new connection.
    let second = pool
        .send_with(|| get(addr, "/second"))
        .await
        .map(|r| r.status());
    assert_eq!(first.await.unwrap(), Ok(http::StatusCode::OK), "processed");
    assert_eq!(second, Ok(http::StatusCode::OK), "sent again");
    assert_eq!(conns.load(Ordering::SeqCst), 2);
    assert_eq!(pool.opened(), 2);
    // Nothing new goes on the drained connection.
    assert_eq!(pool.send(get(addr, "/third")).await.unwrap().status(), 200);
    assert_eq!(conns.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_refused_stream_is_reported_as_safe_to_resend() {
    let (addr, _conns) = goaway_origin().await;
    let pool: Pool<Full<Bytes>> = Pool::new(PoolLimits::default(), Settings::default());
    let first = {
        let pool = pool.clone();
        tokio::spawn(async move { pool.send(get(addr, "/first")).await.map(|r| r.status()) })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let second = pool.send(get(addr, "/second")).await.map(|r| r.status());
    assert_eq!(second, Err(PoolError::Refused));
    assert_eq!(first.await.unwrap(), Ok(http::StatusCode::OK));
}

#[tokio::test]
async fn a_graceful_goaway_lets_open_streams_finish_and_new_ones_move() {
    // The edge listener's own shutdown: GOAWAY, open streams finish.
    let handler = Counting::new(Duration::from_millis(300));
    let (addr, stop) = h2c_server(Arc::clone(&handler), 100).await;
    let pool: Pool<Full<Bytes>> = Pool::new(PoolLimits::default(), Settings::default());
    let open = {
        let pool = pool.clone();
        tokio::spawn(async move {
            let resp = pool.send(get(addr, "/slow")).await?;
            Ok::<_, PoolError>(text(resp).await)
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = stop.send(());
    assert_eq!(open.await.unwrap(), Ok("HTTP/2.0 /slow".to_owned()));
    // The listener is gone, so a new stream cannot connect.
    let after = pool.send(get(addr, "/after")).await.map(|r| r.status());
    assert!(
        matches!(after, Err(PoolError::Connect | PoolError::Refused)),
        "{after:?}"
    );
}

#[tokio::test]
async fn tls_upstreams_negotiate_h2_over_alpn() {
    let handler = Counting::new(Duration::ZERO);
    let shared = Shared {
        http2: true,
        ..Shared::default()
    };
    let snapshot = Snapshot::load_with(&tls_config(vec![cert("exact", &[])]), &shared).unwrap();
    let acceptor = Acceptor::new(Store::new(Arc::new(snapshot)), None);
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let limits = ServerLimits {
        http2: Some(server_settings(100)),
        ..ServerLimits::default()
    };
    tokio::spawn(serve_tls(
        socket,
        acceptor,
        Arc::clone(&handler),
        limits,
        std::future::pending(),
    ));
    let tls = TlsTo::load(&UpstreamTls {
        ca_file: fixture("ca.crt"),
        server_name: Some("api.example.com".to_owned()),
        cert_file: None,
        key_file: None,
    })
    .unwrap();
    let pool: Pool<Full<Bytes>> = Pool::with_tls(PoolLimits::default(), Settings::default(), tls);
    for i in 0..5 {
        let resp = pool.send(get(addr, &format!("/{i}"))).await.unwrap();
        assert_eq!(resp.version(), Version::HTTP_2);
        assert_eq!(text(resp).await, format!("HTTP/2.0 /{i}"));
    }
    assert_eq!(handler.connections(), 1);

    // An origin that speaks only HTTP/1.1 is refused, never downgraded
    // silently by the pool.
    let shared = Shared::default();
    let snapshot = Snapshot::load_with(&tls_config(vec![cert("exact", &[])]), &shared).unwrap();
    let acceptor = Acceptor::new(Store::new(Arc::new(snapshot)), None);
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let h1 = socket.local_addr().unwrap();
    tokio::spawn(serve_tls(
        socket,
        acceptor,
        Counting::new(Duration::ZERO),
        ServerLimits::default(),
        std::future::pending(),
    ));
    let refused = pool.send(get(h1, "/")).await.map(|r| r.status());
    assert_eq!(refused, Err(PoolError::Connect));
}

#[tokio::test]
async fn unreachable_origins_fail_to_connect() {
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    drop(socket);
    let pool: Pool<Full<Bytes>> = Pool::new(PoolLimits::default(), Settings::default());
    assert_eq!(
        pool.send(get(addr, "/")).await.map(|r| r.status()),
        Err(PoolError::Connect)
    );
    // An HTTP/1.1-only cleartext origin answers the preface in HTTP/1.1,
    // not with SETTINGS: it does not speak HTTP/2.
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let h1 = socket.local_addr().unwrap();
    tokio::spawn(serve(
        socket,
        Counting::new(Duration::ZERO),
        ServerLimits::default(),
        std::future::pending(),
    ));
    let r = pool.send(get(h1, "/")).await.map(|r| r.status());
    assert_eq!(r, Err(PoolError::NotHttp2));
    // So does a TLS origin that negotiates only http/1.1, when the pool
    // offers both.
    let snapshot =
        Snapshot::load_with(&tls_config(vec![cert("exact", &[])]), &Shared::default()).unwrap();
    let acceptor = Acceptor::new(Store::new(Arc::new(snapshot)), None);
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let h1s = socket.local_addr().unwrap();
    tokio::spawn(serve_tls(
        socket,
        acceptor,
        Counting::new(Duration::ZERO),
        ServerLimits::default(),
        std::future::pending(),
    ));
    let tls = TlsTo::load(&UpstreamTls {
        ca_file: fixture("ca.crt"),
        server_name: Some("api.example.com".to_owned()),
        cert_file: None,
        key_file: None,
    })
    .unwrap();
    let auto: Pool<Full<Bytes>> =
        Pool::with_tls_auto(PoolLimits::default(), Settings::default(), tls);
    let r = auto.send(get(h1s, "/")).await.map(|r| r.status());
    assert_eq!(r, Err(PoolError::NotHttp2));
}
