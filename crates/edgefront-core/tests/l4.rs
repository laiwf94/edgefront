//! The TCP relay over real sockets: pacer's `tests/it_proxy.c` and the
//! relay cases of `tests/it_failures.c`, run against `l4::proxy` behind a
//! plain accept loop, with the upstream modes of pacer's
//! `tests/upstream.c`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use edgefront_core::l4::{self, CloseReason, Outcome, RelayConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// What a test upstream does with each connection.
#[derive(Clone, Copy)]
enum Mode {
    /// Echo until EOF, then close.
    Echo,
    /// Read to EOF, reply "<fnv64 hex> <count>\n", close.
    Hash,
    /// Send this many pattern bytes, then close.
    Source(u64),
    /// Read this many bytes, then reset the connection.
    Reset(usize),
    /// Accept, then neither read nor write.
    Stall,
}

struct Upstream {
    addr: SocketAddr,
    /// Bytes the upstream has written, over every connection.
    sent: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn upstream(mode: Mode) -> Upstream {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let sent = Arc::new(AtomicU64::new(0));
    let counter = sent.clone();
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let sent = counter.clone();
            match mode {
                Mode::Stall => held.push(s),
                _ => {
                    tokio::spawn(async move { serve(&mut s, mode, &sent).await });
                }
            }
        }
    });
    Upstream { addr, sent, task }
}

async fn serve(s: &mut TcpStream, mode: Mode, sent: &AtomicU64) {
    match mode {
        Mode::Echo => {
            let mut buf = vec![0u8; 65536];
            loop {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if s.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
        Mode::Hash => {
            let mut buf = vec![0u8; 65536];
            let (mut h, mut count) = (FNV_START, 0u64);
            loop {
                match s.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        h = fnv(h, &buf[..n]);
                        count += n as u64;
                    }
                    Err(_) => return,
                }
            }
            let _ = s.write_all(format!("{h:016x} {count}\n").as_bytes()).await;
        }
        Mode::Source(total) => {
            let mut buf = vec![0u8; 65536];
            let mut off = 0;
            while off < total {
                let n = (total - off).min(buf.len() as u64) as usize;
                pattern(&mut buf[..n], off);
                if s.write_all(&buf[..n]).await.is_err() {
                    return;
                }
                off += n as u64;
                sent.fetch_add(n as u64, Ordering::Relaxed);
            }
        }
        Mode::Reset(n) => {
            // Closing with a zero linger sends a reset, and no FIN before it.
            let mut buf = vec![0u8; n];
            if s.read_exact(&mut buf).await.is_ok() {
                s.set_zero_linger().unwrap();
            }
            return;
        }
        Mode::Stall => unreachable!(),
    }
    let _ = s.shutdown().await;
}

/// A proxy listener in front of `to`: every accepted connection runs
/// `l4::proxy`, and its outcome arrives on the receiver.
struct Proxy {
    addr: SocketAddr,
    outcomes: mpsc::UnboundedReceiver<Outcome>,
    task: JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Proxy {
    async fn next(&mut self) -> Outcome {
        tokio::time::timeout(Duration::from_secs(30), self.outcomes.recv())
            .await
            .expect("no outcome within 30 s")
            .unwrap()
    }
}

async fn proxy(to: SocketAddr, cfg: RelayConfig) -> Proxy {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, outcomes) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(l4::proxy(s, to, &cfg).await);
            });
        }
    });
    Proxy {
        addr,
        outcomes,
        task,
    }
}

fn config(buffer: usize, connect: Duration, idle: Duration) -> RelayConfig {
    RelayConfig::new(buffer, connect, idle).unwrap()
}

const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;

fn fnv(mut h: u64, data: &[u8]) -> u64 {
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Deterministic test bytes: the stream at offset `off`.
fn pattern(buf: &mut [u8], off: u64) {
    for (i, b) in buf.iter_mut().enumerate() {
        let x = off + i as u64;
        *b = (x ^ (x >> 8) ^ (x >> 16)) as u8;
    }
}

async fn read_to_eof(s: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut v = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), s.read_to_end(&mut v))
        .await
        .expect("no EOF within 30 s")?;
    Ok(v)
}

#[tokio::test]
async fn echo_round_trip() {
    let up = upstream(Mode::Echo).await;
    let mut p = proxy(up.addr, RelayConfig::default()).await;
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    c.write_all(b"hello pacer").await.unwrap();
    let mut buf = [0u8; 11];
    c.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello pacer");
    c.shutdown().await.unwrap();
    assert!(read_to_eof(&mut c).await.unwrap().is_empty());
    assert_eq!(
        p.next().await,
        Outcome {
            reason: CloseReason::Done,
            bytes_up: 11,
            bytes_down: 11
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_upload_arrives_intact_with_half_close() {
    // 64 MiB up, then FIN; the upstream answers only after it sees EOF, so
    // the half-closed connection must still carry the reply.
    const TOTAL: u64 = 64 << 20;
    let up = upstream(Mode::Hash).await;
    let cfg = config(64 << 10, Duration::from_secs(5), Duration::from_secs(300));
    let mut p = proxy(up.addr, cfg).await;
    let c = TcpStream::connect(p.addr).await.unwrap();
    let (mut r, mut w) = c.into_split();
    let sender = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        let mut h = FNV_START;
        let mut off = 0;
        while off < TOTAL {
            pattern(&mut buf, off);
            h = fnv(h, &buf);
            w.write_all(&buf).await.unwrap();
            off += buf.len() as u64;
        }
        w.shutdown().await.unwrap();
        h
    });
    let mut reply = String::new();
    tokio::time::timeout(Duration::from_secs(60), r.read_to_string(&mut reply))
        .await
        .unwrap()
        .unwrap();
    let h = sender.await.unwrap();
    assert_eq!(reply, format!("{h:016x} {TOTAL}\n"));
    let out = p.next().await;
    assert_eq!(out.reason, CloseReason::Done);
    assert_eq!(out.bytes_up, TOTAL);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_download_arrives_intact() {
    const TOTAL: u64 = 64 << 20;
    let up = upstream(Mode::Source(TOTAL)).await;
    let mut p = proxy(up.addr, RelayConfig::default()).await;
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    let mut buf = vec![0u8; 65536];
    let (mut got, mut h) = (0u64, FNV_START);
    loop {
        let n = c.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        h = fnv(h, &buf[..n]);
        got += n as u64;
    }
    let mut want = FNV_START;
    let mut off = 0;
    while off < TOTAL {
        pattern(&mut buf, off);
        want = fnv(want, &buf);
        off += buf.len() as u64;
    }
    assert_eq!((got, h), (TOTAL, want));
    c.shutdown().await.unwrap();
    let out = p.next().await;
    assert_eq!(out.reason, CloseReason::Done);
    assert_eq!((out.bytes_up, out.bytes_down), (0, TOTAL));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_reader_does_not_make_the_proxy_buffer_more() {
    // The upstream pushes 256 MiB at a client that does not read for a
    // while. The proxy stops reading from the upstream once its one buffer
    // is full, so the upstream can only get as far as the kernel buffers on
    // both sides allow. pacer measures this through pacerd's RSS; here the
    // upstream counts what it managed to send.
    const TOTAL: u64 = 256 << 20;
    let up = upstream(Mode::Source(TOTAL)).await;
    let mut p = proxy(up.addr, RelayConfig::default()).await;
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let stalled = up.sent.load(Ordering::Relaxed);
    assert!(stalled > 0);
    assert!(
        stalled < 32 << 20,
        "the upstream got {stalled} bytes out to a client that reads nothing"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let later = up.sent.load(Ordering::Relaxed);
    // Once the buffers are full nothing more moves.
    assert!(later - stalled <= 1 << 20, "{stalled} then {later}");
    let mut buf = vec![0u8; 1 << 16];
    let mut got = 0u64;
    loop {
        let n = c.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        got += n as u64;
    }
    assert_eq!(got, TOTAL);
    c.shutdown().await.unwrap();
    assert_eq!(p.next().await.reason, CloseReason::Done);
}

#[tokio::test]
async fn an_upstream_that_refuses_closes_the_client() {
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    let mut p = proxy(dead, RelayConfig::default()).await;
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    assert!(read_to_eof(&mut c).await.unwrap().is_empty());
    assert_eq!(p.next().await.reason, CloseReason::UpstreamRefused);
}

#[tokio::test]
async fn an_upstream_connect_that_hangs_times_out() {
    // A listener that never accepts, with its tiny backlog already full:
    // further SYNs are dropped and the connect hangs.
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let blocked = socket.listen(0).unwrap();
    let addr = blocked.local_addr().unwrap();
    let mut fillers = Vec::new();
    for _ in 0..4 {
        let s = TcpSocket::new_v4().unwrap();
        fillers.push(tokio::spawn(async move { s.connect(addr).await }));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let cfg = config(
        16 << 10,
        Duration::from_millis(300),
        Duration::from_secs(300),
    );
    let mut p = proxy(addr, cfg).await;
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    let t0 = Instant::now();
    assert!(read_to_eof(&mut c).await.unwrap().is_empty());
    let took = t0.elapsed();
    assert!(took >= Duration::from_millis(250), "{took:?}");
    assert!(took < Duration::from_secs(3), "{took:?}");
    assert_eq!(p.next().await.reason, CloseReason::ConnectTimeout);
    for f in fillers {
        f.abort();
    }
    drop(blocked);
}

#[tokio::test]
async fn the_connect_helper_names_its_failures() {
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    assert_eq!(
        l4::connect(dead, Duration::from_secs(5)).await.unwrap_err(),
        CloseReason::UpstreamRefused
    );
    // A TCP connect to the broadcast address fails at once, with
    // ENETUNREACH or EACCES depending on the host's routes.
    let broadcast: SocketAddr = "255.255.255.255:9".parse().unwrap();
    assert_eq!(
        l4::connect(broadcast, Duration::from_secs(5))
            .await
            .unwrap_err(),
        CloseReason::UpstreamUnreachable
    );
    let up = upstream(Mode::Echo).await;
    let s = l4::connect(up.addr, Duration::from_secs(5)).await.unwrap();
    assert!(s.nodelay().unwrap());
}

#[tokio::test]
async fn an_ipv6_upstream_is_reached_when_the_host_has_ipv6() {
    let Ok(l) = TcpListener::bind("[::1]:0").await else {
        // No IPv6 loopback on this host: nothing to test.
        return;
    };
    let addr = l.local_addr().unwrap();
    let accept = tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        s.write_all(b"v6").await.unwrap();
    });
    let mut s = l4::connect(addr, Duration::from_secs(5)).await.unwrap();
    let mut b = [0u8; 2];
    s.read_exact(&mut b).await.unwrap();
    assert_eq!(&b, b"v6");
    accept.await.unwrap();
}

#[tokio::test]
async fn an_upstream_reset_is_reported() {
    let up = upstream(Mode::Reset(5)).await;
    let mut p = proxy(up.addr, RelayConfig::default()).await;
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    c.write_all(b"12345").await.unwrap();
    let mut buf = [0u8; 8];
    let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
        .await
        .unwrap();
    assert!(matches!(n, Ok(0) | Err(_)), "{n:?}");
    let out = p.next().await;
    assert_eq!(out.reason, CloseReason::UpstreamReset);
    assert_eq!(out.bytes_up, 5);
}

#[tokio::test]
async fn a_client_reset_is_reported() {
    let up = upstream(Mode::Echo).await;
    let mut p = proxy(up.addr, RelayConfig::default()).await;
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    c.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    c.read_exact(&mut buf).await.unwrap();
    c.set_zero_linger().unwrap();
    drop(c);
    let out = p.next().await;
    assert_eq!(out.reason, CloseReason::ClientReset);
    assert_eq!((out.bytes_up, out.bytes_down), (4, 4));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_concurrent_clients() {
    const N: usize = 64;
    let up = upstream(Mode::Echo).await;
    let mut p = proxy(up.addr, RelayConfig::default()).await;
    let addr = p.addr;
    let clients: Vec<_> = (0..N)
        .map(|k| {
            tokio::spawn(async move {
                let mut c = TcpStream::connect(addr).await.unwrap();
                let mut sent = 0u64;
                for i in 0..50 {
                    let out = format!("message {i} from {k}");
                    c.write_all(out.as_bytes()).await.unwrap();
                    let mut back = vec![0u8; out.len()];
                    c.read_exact(&mut back).await.unwrap();
                    assert_eq!(back, out.as_bytes());
                    sent += out.len() as u64;
                }
                c.shutdown().await.unwrap();
                assert!(read_to_eof(&mut c).await.unwrap().is_empty());
                sent
            })
        })
        .collect();
    let mut total = 0;
    for c in clients {
        total += c.await.unwrap();
    }
    let (mut up_bytes, mut down_bytes) = (0, 0);
    for _ in 0..N {
        let out = p.next().await;
        assert_eq!(out.reason, CloseReason::Done);
        up_bytes += out.bytes_up;
        down_bytes += out.bytes_down;
    }
    assert_eq!((up_bytes, down_bytes), (total, total));
}

#[tokio::test]
async fn a_stalled_upstream_hits_the_idle_timeout() {
    // The upstream accepts and then goes silent, as behind a partition: the
    // connection is closed by the idle timeout, not held for ever.
    let up = upstream(Mode::Stall).await;
    let cfg = config(16 << 10, Duration::from_secs(5), Duration::from_secs(1));
    let mut p = proxy(up.addr, cfg).await;
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    c.write_all(b"hello").await.unwrap();
    let t0 = Instant::now();
    assert!(read_to_eof(&mut c).await.unwrap().is_empty());
    let ms = t0.elapsed();
    assert!(ms >= Duration::from_millis(800), "{ms:?}");
    assert!(ms < Duration::from_millis(2500), "{ms:?}");
    assert_eq!(
        p.next().await,
        Outcome {
            reason: CloseReason::IdleTimeout,
            bytes_up: 5,
            bytes_down: 0
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_sending_into_a_stalled_upstream_blocks() {
    // The client keeps sending into a stalled upstream: the proxy's buffer
    // and the socket buffers fill and the client blocks; the proxy never
    // buffers more than its one buffer per direction.
    let up = upstream(Mode::Stall).await;
    let cfg = config(16 << 10, Duration::from_secs(5), Duration::from_secs(1));
    let mut p = proxy(up.addr, cfg).await;
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    let chunk = vec![0u8; 65536];
    let mut sent = 0u64;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(700) {
        match tokio::time::timeout(Duration::from_millis(100), c.write(&chunk)).await {
            Ok(Ok(n)) => sent += n as u64,
            Ok(Err(e)) => panic!("{e}"),
            Err(_) => {}
        }
    }
    // Kernel buffers on both sides plus one 16 KiB proxy buffer.
    assert!(sent < 32 << 20, "{sent}");
    let out = p.next().await;
    assert_eq!(out.reason, CloseReason::IdleTimeout);
    assert!(out.bytes_up <= sent);
    assert!(sent - out.bytes_up < 32 << 20);
}

#[tokio::test]
async fn the_upstream_closes_first_while_the_client_is_open() {
    // The upstream answers 5 bytes and closes without reading; the client
    // still gets the 5 bytes and then EOF.
    let up = upstream(Mode::Source(5)).await;
    let mut p = proxy(up.addr, RelayConfig::default()).await;
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    assert_eq!(read_to_eof(&mut c).await.unwrap().len(), 5);
    drop(c);
    let out = p.next().await;
    assert_eq!(out.reason, CloseReason::Done);
    assert_eq!(out.bytes_down, 5);
}
