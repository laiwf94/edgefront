//! HTTP/2 flow control over a simulated long link: a 100 MB download
//! through the upstream pool over 25 MB/s (200 Mbit/s) with 50 ms of
//! latency each way (a 100 ms round trip) reaches 90% of what plain TCP
//! gets over the same link with adaptive windows, and is capped by its
//! window without them. The link's bandwidth-delay product, 2.5 MB, is
//! past the pool's 1 MiB starting window, so the estimator has to grow
//! it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use edgefront_core::config::Http2;
use edgefront_core::h2::flow::{bdp, window_limit};
use edgefront_core::h2::{Pool, PoolLimits, Settings};
use edgefront_core::listener::{Handler, ServerLimits, serve};
use http::{Request, Response};
use http_body_util::{BodyExt, Empty};
use hyper::body::{Frame, Incoming};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::Instant as TokioInstant;

const RATE: u64 = 25_000_000;
const ONE_WAY: Duration = Duration::from_millis(50);
const RTT: Duration = Duration::from_millis(100);
const SIZE: usize = 100_000_000;

/// Carries bytes from `from` to `to` as a link would: each chunk leaves
/// once the link has serialised the ones before it at `RATE`, and
/// arrives `ONE_WAY` later. The queue is unbounded, so the link, not TCP
/// between the test's sockets, is the bottleneck.
async fn carry(mut from: tokio::net::tcp::OwnedReadHalf, mut to: tokio::net::tcp::OwnedWriteHalf) {
    let (tx, mut rx) = mpsc::unbounded_channel::<(TokioInstant, Bytes)>();
    let writer = tokio::spawn(async move {
        let mut free = TokioInstant::now();
        while let Some((read_at, chunk)) = rx.recv().await {
            let start = free.max(read_at);
            free = start + Duration::from_nanos(chunk.len() as u64 * 1_000_000_000 / RATE);
            tokio::time::sleep_until(free + ONE_WAY).await;
            if to.write_all(&chunk).await.is_err() {
                return;
            }
        }
        let _ = to.shutdown().await;
    });
    let mut buf = vec![0u8; 16 << 10];
    loop {
        match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx
                    .send((TokioInstant::now(), Bytes::copy_from_slice(&buf[..n])))
                    .is_err()
                {
                    break;
                }
            }
        }
    }
    drop(tx);
    let _ = writer.await;
}

/// A simulated link in front of `target`.
async fn link(target: SocketAddr) -> SocketAddr {
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (client, _) = socket.accept().await.unwrap();
            let server = TcpStream::connect(target).await.unwrap();
            let _ = (client.set_nodelay(true), server.set_nodelay(true));
            let (cr, cw) = client.into_split();
            let (sr, sw) = server.into_split();
            tokio::spawn(carry(cr, sw));
            tokio::spawn(carry(sr, cw));
        }
    });
    addr
}

mod tokio_stream_like {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// `left` bytes as 64 KiB frames of one shared buffer.
    pub struct Chunks {
        pub left: usize,
        pub chunk: Bytes,
    }

    impl hyper::body::Body for Chunks {
        type Data = Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            if self.left == 0 {
                return Poll::Ready(None);
            }
            let n = self.left.min(self.chunk.len());
            self.left -= n;
            Poll::Ready(Some(Ok(Frame::data(self.chunk.slice(..n)))))
        }
    }
}

/// Serves `GET /<bytes>` with that many bytes.
struct Download;

impl Handler for Download {
    type Body = tokio_stream_like::Chunks;

    async fn call(self: Arc<Self>, req: Request<Incoming>, _: SocketAddr) -> Response<Self::Body> {
        let left = req.uri().path()[1..].parse().unwrap_or(0);
        Response::new(tokio_stream_like::Chunks {
            left,
            chunk: Bytes::from(vec![b'x'; 64 << 10]),
        })
    }
}

async fn origin() -> SocketAddr {
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let limits = ServerLimits {
        http2: Some(Settings::from(&Http2 {
            cleartext: true,
            ..Http2::default()
        })),
        ..ServerLimits::default()
    };
    tokio::spawn(serve(
        socket,
        Arc::new(Download),
        limits,
        std::future::pending(),
    ));
    addr
}

/// Bytes per second of a download of `size` through `pool`.
async fn download(pool: &Pool<Empty<Bytes>>, via: SocketAddr, size: usize) -> f64 {
    let start = Instant::now();
    let req = Request::get(format!("http://{via}/{size}"))
        .body(Empty::new())
        .unwrap();
    let mut body = pool.send(req).await.unwrap().into_body();
    let mut got = 0;
    while let Some(frame) = body.frame().await {
        got += frame.unwrap().data_ref().map_or(0, Bytes::len);
    }
    assert_eq!(got, size);
    size as f64 / start.elapsed().as_secs_f64()
}

/// Bytes per second of plain TCP over the link: the link speed.
async fn tcp_speed(size: usize) -> f64 {
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = socket.accept().await.unwrap();
        let mut ask = [0u8; 1];
        s.read_exact(&mut ask).await.unwrap();
        let chunk = vec![b'x'; 64 << 10];
        let mut left = size;
        while left > 0 {
            let n = left.min(chunk.len());
            s.write_all(&chunk[..n]).await.unwrap();
            left -= n;
        }
    });
    let via = link(addr).await;
    let mut c = TcpStream::connect(via).await.unwrap();
    // Timed from the request, as the HTTP/2 download is, so both pay one
    // round trip before the first byte.
    let start = Instant::now();
    c.write_all(b"g").await.unwrap();
    let mut buf = vec![0u8; 1 << 20];
    let mut got = 0;
    while got < size {
        let n = c.read(&mut buf).await.unwrap();
        assert!(n > 0);
        got += n;
    }
    size as f64 / start.elapsed().as_secs_f64()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adaptive_windows_fill_a_long_fat_link() {
    let link_speed = tcp_speed(SIZE).await;
    let via = link(origin().await).await;
    let adaptive = Pool::new(
        PoolLimits::default(),
        Settings {
            adaptive_window: true,
            ..Settings::default()
        },
    );
    let speed = download(&adaptive, via, SIZE).await;
    let share = speed / link_speed;
    eprintln!(
        "100 MB over {} MB/s with a {} ms round trip: tcp {:.1} MB/s, adaptive http/2 {:.1} MB/s ({:.1}%), bdp {} bytes",
        RATE / 1_000_000,
        RTT.as_millis(),
        link_speed / 1e6,
        speed / 1e6,
        share * 100.0,
        bdp(RATE, RTT)
    );
    assert!(share >= 0.9, "{:.1}% of the link", share * 100.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fixed_windows_cap_a_long_fat_link() {
    let via = link(origin().await).await;
    // The protocol's default window: at most 655 KB/s over 100 ms.
    let fixed = Pool::new(
        PoolLimits::default(),
        Settings {
            initial_stream_window: 65_535,
            initial_connection_window: 65_535,
            ..Settings::default()
        },
    );
    let size = 2_000_000;
    let speed = download(&fixed, via, size).await;
    let cap = window_limit(65_535, RTT) as f64;
    eprintln!(
        "2 MB with fixed 64 KiB windows: {:.2} MB/s, the window allows {:.2} MB/s",
        speed / 1e6,
        cap / 1e6
    );
    assert!(speed <= cap * 1.1, "{speed} over the window's {cap}");
}
