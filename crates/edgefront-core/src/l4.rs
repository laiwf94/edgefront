//! The base TCP relay: one client connection joined to one upstream
//! connection, with bytes copied both ways through one fixed buffer per
//! direction. This is the data path of pacerd's `src/pacerd/proxy.c`,
//! moved onto tokio.
//!
//! What it keeps from pacerd:
//!
//! - **Fixed memory.** Each connection allocates one block of twice the
//!   buffer size when it starts and never more. A direction reads from its
//!   source only while its buffer has room, so a destination that does not
//!   read stops the source being read, and TCP flow control pushes back on
//!   the sender. A buffer that is full with its unwritten bytes at an
//!   offset is moved back to the start before the next read, as in pacerd.
//! - **Half-close.** An end of stream from one side is passed on as a
//!   `shutdown(SHUT_WR)` to the other once every byte before it has been
//!   written, and the other direction carries on. The connection is done
//!   when both directions have been shut this way.
//! - **Bytes sent before the upstream answers.** While the upstream
//!   connect is in progress the client is read into the upstream-bound
//!   buffer (and its end of stream noted), and the bytes are written as
//!   soon as the connect completes.
//! - **Timeouts.** The connect has its own deadline. Once connected, a
//!   connection on which nothing has been read in either direction for the
//!   idle timeout is closed. As in pacerd, only reads count as activity: a
//!   destination that accepts bytes does not keep a silent source alive.
//! - **Close reasons.** Every way a connection ends is a [`CloseReason`]
//!   whose name is pacerd's `reason=` string, so counters and logs can be
//!   compared with pacerd's. A connection reset or broken pipe on a side is
//!   its `_reset` reason, any other I/O error its `_error` reason.
//!
//! The limiter's byte shaping and slow-client check, the listener with its
//! admission limits and drain, and proxy week 12's UDP, TLS passthrough and
//! PROXY protocol build on this module; it has no policy of its own.

use std::fmt;
use std::future::{Future, poll_fn, ready};
use std::io;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpSocket, TcpStream};
use tokio::time::{Instant, sleep_until};

/// Index of the client-to-upstream direction.
const UP: usize = 0;
/// Index of the upstream-to-client direction.
const DOWN: usize = 1;

/// Why a relayed connection ended. [`CloseReason::as_str`] is pacerd's
/// name for each, as it appears after `reason=` in its log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloseReason {
    /// Both sides sent their end of stream and every byte was delivered.
    Done,
    /// Nothing was read from either side for the idle timeout.
    IdleTimeout,
    /// The upstream connect did not complete within the connect timeout.
    ConnectTimeout,
    /// The upstream refused the connection.
    UpstreamRefused,
    /// The upstream connect failed for any other reason.
    UpstreamUnreachable,
    /// No socket could be created for the upstream connect, as when the
    /// process is out of file descriptors.
    UpstreamSocket,
    /// The client reset the connection, or a write to it hit a broken pipe.
    ClientReset,
    /// Any other I/O error on the client side.
    ClientError,
    /// The upstream reset the connection, or a write to it hit a broken
    /// pipe.
    UpstreamReset,
    /// Any other I/O error on the upstream side.
    UpstreamError,
}

impl CloseReason {
    /// Every reason, in the order above.
    pub const ALL: [CloseReason; 10] = [
        CloseReason::Done,
        CloseReason::IdleTimeout,
        CloseReason::ConnectTimeout,
        CloseReason::UpstreamRefused,
        CloseReason::UpstreamUnreachable,
        CloseReason::UpstreamSocket,
        CloseReason::ClientReset,
        CloseReason::ClientError,
        CloseReason::UpstreamReset,
        CloseReason::UpstreamError,
    ];

    /// pacerd's name for the reason.
    pub const fn as_str(self) -> &'static str {
        match self {
            CloseReason::Done => "done",
            CloseReason::IdleTimeout => "idle_timeout",
            CloseReason::ConnectTimeout => "connect_timeout",
            CloseReason::UpstreamRefused => "upstream_refused",
            CloseReason::UpstreamUnreachable => "upstream_unreachable",
            CloseReason::UpstreamSocket => "upstream_socket",
            CloseReason::ClientReset => "client_reset",
            CloseReason::ClientError => "client_error",
            CloseReason::UpstreamReset => "upstream_reset",
            CloseReason::UpstreamError => "upstream_error",
        }
    }

    /// Whether the connection failed because the upstream could not be
    /// reached, which pacerd counts as `closed_upstream_failed`.
    pub const fn is_upstream_failure(self) -> bool {
        matches!(
            self,
            CloseReason::ConnectTimeout
                | CloseReason::UpstreamRefused
                | CloseReason::UpstreamUnreachable
                | CloseReason::UpstreamSocket
        )
    }

    /// Whether the connection ended on a reset or an I/O error on either
    /// side, which pacerd counts as `closed_reset`.
    pub const fn is_reset(self) -> bool {
        matches!(
            self,
            CloseReason::ClientReset
                | CloseReason::ClientError
                | CloseReason::UpstreamReset
                | CloseReason::UpstreamError
        )
    }

    /// The reason for an I/O error on the client side (`client` true) or
    /// the upstream side, as pacerd's `reset_reason` picks it.
    fn of_io(client: bool, err: &io::Error) -> CloseReason {
        let reset = matches!(
            err.kind(),
            io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
        );
        match (client, reset) {
            (true, true) => CloseReason::ClientReset,
            (true, false) => CloseReason::ClientError,
            (false, true) => CloseReason::UpstreamReset,
            (false, false) => CloseReason::UpstreamError,
        }
    }

    /// The reason for a failed upstream connect.
    fn of_connect(err: &io::Error) -> CloseReason {
        if err.kind() == io::ErrorKind::ConnectionRefused {
            CloseReason::UpstreamRefused
        } else {
            CloseReason::UpstreamUnreachable
        }
    }
}

impl fmt::Display for CloseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A [`RelayConfig`] that cannot work: a zero buffer or a zero timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidRelayConfig(&'static str);

impl fmt::Display for InvalidRelayConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} must be greater than zero", self.0)
    }
}

impl std::error::Error for InvalidRelayConfig {}

/// The settings of one relayed connection: pacerd's `buffer`,
/// `connect_timeout` and `idle_timeout`. The limiter's config reader
/// enforces pacerd's bounds on them; this type only rules out values the
/// relay cannot work with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayConfig {
    buffer_size: usize,
    connect_timeout: Duration,
    idle_timeout: Duration,
}

impl RelayConfig {
    /// A config with `buffer_size` bytes of buffer per direction. Each must
    /// be greater than zero.
    pub fn new(
        buffer_size: usize,
        connect_timeout: Duration,
        idle_timeout: Duration,
    ) -> Result<Self, InvalidRelayConfig> {
        if buffer_size == 0 {
            return Err(InvalidRelayConfig("buffer_size"));
        }
        if connect_timeout.is_zero() {
            return Err(InvalidRelayConfig("connect_timeout"));
        }
        if idle_timeout.is_zero() {
            return Err(InvalidRelayConfig("idle_timeout"));
        }
        Ok(RelayConfig {
            buffer_size,
            connect_timeout,
            idle_timeout,
        })
    }

    /// Bytes of buffer per direction.
    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    /// How long the upstream connect may take.
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// How long a connected relay may go without reading anything.
    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// The memory a connection's buffers take: one block of this many bytes.
    pub fn buffer_memory(&self) -> usize {
        self.buffer_size.saturating_mul(2)
    }
}

impl Default for RelayConfig {
    /// pacerd's defaults: 16 KiB per direction, 5 seconds to connect and
    /// 5 minutes idle.
    fn default() -> Self {
        RelayConfig {
            buffer_size: 16 * 1024,
            connect_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(300),
        }
    }
}

/// How a relayed connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// Why it ended.
    pub reason: CloseReason,
    /// Bytes read from the client (pacerd's `bytes_up`).
    pub bytes_up: u64,
    /// Bytes read from the upstream (pacerd's `bytes_down`).
    pub bytes_down: u64,
}

/// Opens a TCP connection to `addr` with Nagle's algorithm off, within
/// `timeout`. Errors are the close reasons pacerd gives a failed connect.
pub async fn connect(addr: SocketAddr, timeout: Duration) -> Result<TcpStream, CloseReason> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()
    } else {
        TcpSocket::new_v6()
    }
    .map_err(|_| CloseReason::UpstreamSocket)?;
    // Small writes are forwarded as they come; Nagle would add latency on
    // top of the peer's own. Failure only costs latency.
    let _ = socket.set_nodelay(true);
    match tokio::time::timeout(timeout, socket.connect(addr)).await {
        Err(_) => Err(CloseReason::ConnectTimeout),
        Ok(Err(e)) => Err(CloseReason::of_connect(&e)),
        Ok(Ok(stream)) => Ok(stream),
    }
}

/// Relays an accepted client connection to the upstream at `upstream`:
/// connects within the config's connect timeout, reading what the client
/// sends meanwhile, then copies both ways until the connection is done or
/// fails. Both sockets are closed when this returns.
pub async fn proxy(client: TcpStream, upstream: SocketAddr, cfg: &RelayConfig) -> Outcome {
    let _ = client.set_nodelay(true);
    proxy_with(client, connect(upstream, cfg.connect_timeout), cfg).await
}

/// Relays between two connected streams until the connection is done or
/// fails.
pub async fn relay<C, U>(client: C, upstream: U, cfg: &RelayConfig) -> Outcome
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    proxy_with(client, ready(Ok(upstream)), cfg).await
}

/// Relays `client` to the stream `connect` resolves to. While `connect` is
/// pending the client is read into the upstream-bound buffer; a client
/// reset or error in that time ends the relay with its reason and drops
/// `connect`. A connect that fails ends it with the connect's reason.
/// `connect` should carry its own deadline, as [`connect`] does.
pub async fn proxy_with<C, U, F>(mut client: C, connect: F, cfg: &RelayConfig) -> Outcome
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
    F: Future<Output = Result<U, CloseReason>>,
{
    let mut pipe = Pipe::new(cfg.buffer_size);
    let mut connect = pin!(connect);
    let connected = poll_fn(|cx| {
        if let Poll::Ready(r) = connect.as_mut().poll(cx) {
            return Poll::Ready(r);
        }
        match pipe.fill(UP, &mut client, cx) {
            Ok(()) => Poll::Pending,
            Err(reason) => Poll::Ready(Err(reason)),
        }
    })
    .await;
    let mut upstream = match connected {
        Ok(u) => u,
        Err(reason) => return pipe.outcome(reason),
    };
    pipe.last_io = Instant::now();
    let idle_timeout = cfg.idle_timeout;
    let mut idle = pin!(sleep_until(deadline(pipe.last_io, idle_timeout)));
    let reason = poll_fn(|cx| {
        if let Err(reason) = pipe.pump(UP, &mut client, &mut upstream, cx) {
            return Poll::Ready(reason);
        }
        if let Err(reason) = pipe.pump(DOWN, &mut upstream, &mut client, cx) {
            return Poll::Ready(reason);
        }
        if pipe.shut[UP] && pipe.shut[DOWN] {
            return Poll::Ready(CloseReason::Done);
        }
        // The timer is moved only when it fires: a busy connection costs
        // one clock read per read, not a timer update.
        while idle.as_mut().poll(cx).is_ready() {
            let due = deadline(pipe.last_io, idle_timeout);
            if due <= Instant::now() {
                return Poll::Ready(CloseReason::IdleTimeout);
            }
            idle.as_mut().reset(due);
        }
        Poll::Pending
    })
    .await;
    pipe.outcome(reason)
}

/// `from + d`, or a time so far ahead that it never comes when that
/// cannot be represented.
fn deadline(from: Instant, d: Duration) -> Instant {
    from.checked_add(d)
        .unwrap_or_else(|| from + Duration::from_secs(86400 * 365 * 30))
}

/// Both directions' buffers and state.
struct Pipe {
    /// One allocation: the upstream-bound buffer, then the client-bound.
    mem: Box<[u8]>,
    cap: usize,
    /// Unwritten bytes of direction `d` are `start[d]..end[d]` of its
    /// buffer.
    start: [usize; 2],
    end: [usize; 2],
    /// The direction's source has sent its end of stream.
    eof: [bool; 2],
    /// The end of stream has been passed on to the destination.
    shut: [bool; 2],
    /// Bytes read from the direction's source.
    bytes: [u64; 2],
    /// When anything was last read.
    last_io: Instant,
}

impl Pipe {
    fn new(cap: usize) -> Self {
        Pipe {
            mem: vec![0; cap * 2].into_boxed_slice(),
            cap,
            start: [0; 2],
            end: [0; 2],
            eof: [false; 2],
            shut: [false; 2],
            bytes: [0; 2],
            last_io: Instant::now(),
        }
    }

    fn outcome(&self, reason: CloseReason) -> Outcome {
        Outcome {
            reason,
            bytes_up: self.bytes[UP],
            bytes_down: self.bytes[DOWN],
        }
    }

    /// Reads direction `dir`'s source into its buffer until the buffer is
    /// full, the source has nothing more for now, or it ends. When it
    /// returns with room left and no end of stream, the source's waker is
    /// registered.
    fn fill<S: AsyncRead + Unpin>(
        &mut self,
        dir: usize,
        src: &mut S,
        cx: &mut Context<'_>,
    ) -> Result<(), CloseReason> {
        while !self.eof[dir] {
            if self.end[dir] == self.cap {
                if self.start[dir] == 0 {
                    return Ok(());
                }
                // Move the unwritten bytes to the front, as pacerd does
                // when its buffer runs out of tail room.
                let base = dir * self.cap;
                self.mem
                    .copy_within(base + self.start[dir]..base + self.end[dir], base);
                self.end[dir] -= self.start[dir];
                self.start[dir] = 0;
            }
            let base = dir * self.cap;
            let mut rb = ReadBuf::new(&mut self.mem[base + self.end[dir]..base + self.cap]);
            match Pin::new(&mut *src).poll_read(cx, &mut rb) {
                Poll::Pending => return Ok(()),
                Poll::Ready(Err(e)) => return Err(CloseReason::of_io(dir == UP, &e)),
                Poll::Ready(Ok(())) => {
                    let n = rb.filled().len();
                    if n == 0 {
                        self.eof[dir] = true;
                    } else {
                        self.end[dir] += n;
                        self.bytes[dir] += n as u64;
                        self.last_io = Instant::now();
                    }
                }
            }
        }
        Ok(())
    }

    /// Moves direction `dir` along: writes what its buffer holds, passes
    /// an end of stream on once the buffer is empty, and reads more while
    /// there is room. Returns once nothing more can happen without a wake,
    /// with every waker it waits on registered.
    fn pump<S, D>(
        &mut self,
        dir: usize,
        src: &mut S,
        dst: &mut D,
        cx: &mut Context<'_>,
    ) -> Result<(), CloseReason>
    where
        S: AsyncRead + Unpin,
        D: AsyncWrite + Unpin,
    {
        // The destination is the client for the client-bound direction.
        let dst_is_client = dir == DOWN;
        loop {
            let base = dir * self.cap;
            let mut blocked = false;
            while self.start[dir] < self.end[dir] {
                let data = &self.mem[base + self.start[dir]..base + self.end[dir]];
                match Pin::new(&mut *dst).poll_write(cx, data) {
                    Poll::Pending => {
                        blocked = true;
                        break;
                    }
                    Poll::Ready(Ok(0)) => {
                        let e = io::Error::from(io::ErrorKind::WriteZero);
                        return Err(CloseReason::of_io(dst_is_client, &e));
                    }
                    Poll::Ready(Ok(n)) => self.start[dir] += n,
                    Poll::Ready(Err(e)) => return Err(CloseReason::of_io(dst_is_client, &e)),
                }
            }
            if !blocked {
                self.start[dir] = 0;
                self.end[dir] = 0;
                if self.eof[dir] && !self.shut[dir] {
                    // Pass the FIN on once everything before it has been
                    // written. Like pacerd, a failed shutdown is not an
                    // error: the peer is gone and the other direction will
                    // say so.
                    match Pin::new(&mut *dst).poll_shutdown(cx) {
                        Poll::Pending => return Ok(()),
                        Poll::Ready(_) => self.shut[dir] = true,
                    }
                }
            }
            if self.eof[dir] {
                return Ok(());
            }
            let before = self.bytes[dir];
            self.fill(dir, src, cx)?;
            if self.bytes[dir] == before && !self.eof[dir] {
                // Nothing was read: the source is pending (its waker is
                // registered) or the buffer is full behind a blocked
                // write (the destination's waker is).
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use proptest::prelude::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    fn cfg(buffer: usize) -> RelayConfig {
        RelayConfig::new(buffer, Duration::from_secs(5), Duration::from_secs(300)).unwrap()
    }

    #[test]
    fn reason_names_are_pacerds() {
        let names: Vec<&str> = CloseReason::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            names,
            [
                "done",
                "idle_timeout",
                "connect_timeout",
                "upstream_refused",
                "upstream_unreachable",
                "upstream_socket",
                "client_reset",
                "client_error",
                "upstream_reset",
                "upstream_error",
            ]
        );
        for r in CloseReason::ALL {
            assert_eq!(r.to_string(), r.as_str());
        }
    }

    #[test]
    fn reasons_fall_into_pacerds_counters() {
        // pacerd's stat_for: closed_done, closed_idle, closed_upstream_failed
        // and closed_reset.
        let failed: Vec<_> = CloseReason::ALL
            .into_iter()
            .filter(|r| r.is_upstream_failure())
            .collect();
        assert_eq!(
            failed,
            [
                CloseReason::ConnectTimeout,
                CloseReason::UpstreamRefused,
                CloseReason::UpstreamUnreachable,
                CloseReason::UpstreamSocket,
            ]
        );
        let reset: Vec<_> = CloseReason::ALL
            .into_iter()
            .filter(|r| r.is_reset())
            .collect();
        assert_eq!(
            reset,
            [
                CloseReason::ClientReset,
                CloseReason::ClientError,
                CloseReason::UpstreamReset,
                CloseReason::UpstreamError,
            ]
        );
        assert!(!CloseReason::Done.is_reset() && !CloseReason::Done.is_upstream_failure());
        assert!(!CloseReason::IdleTimeout.is_reset());
    }

    #[test]
    fn io_errors_map_as_pacerds_reset_reason() {
        use io::ErrorKind::*;
        let cases = [
            (true, ConnectionReset, CloseReason::ClientReset),
            (true, BrokenPipe, CloseReason::ClientReset),
            (true, ConnectionAborted, CloseReason::ClientError),
            (true, PermissionDenied, CloseReason::ClientError),
            (false, ConnectionReset, CloseReason::UpstreamReset),
            (false, BrokenPipe, CloseReason::UpstreamReset),
            (false, TimedOut, CloseReason::UpstreamError),
            (false, WriteZero, CloseReason::UpstreamError),
        ];
        for (client, kind, want) in cases {
            assert_eq!(CloseReason::of_io(client, &io::Error::from(kind)), want);
        }
        assert_eq!(
            CloseReason::of_connect(&io::Error::from(ConnectionRefused)),
            CloseReason::UpstreamRefused
        );
        assert_eq!(
            CloseReason::of_connect(&io::Error::from(HostUnreachable)),
            CloseReason::UpstreamUnreachable
        );
    }

    #[test]
    fn config_rejects_zeroes_and_defaults_to_pacerds() {
        let d = RelayConfig::default();
        assert_eq!(d.buffer_size(), 16 * 1024);
        assert_eq!(d.connect_timeout(), Duration::from_secs(5));
        assert_eq!(d.idle_timeout(), Duration::from_secs(300));
        assert_eq!(d.buffer_memory(), 32 * 1024);
        let s = Duration::from_secs(1);
        let e = RelayConfig::new(0, s, s).unwrap_err();
        assert_eq!(e.to_string(), "buffer_size must be greater than zero");
        assert_eq!(
            RelayConfig::new(1, Duration::ZERO, s)
                .unwrap_err()
                .to_string(),
            "connect_timeout must be greater than zero"
        );
        assert_eq!(
            RelayConfig::new(1, s, Duration::ZERO)
                .unwrap_err()
                .to_string(),
            "idle_timeout must be greater than zero"
        );
        assert_eq!(
            RelayConfig::new(1, s, s).unwrap(),
            RelayConfig::new(1, s, s).unwrap()
        );
    }

    #[test]
    fn a_deadline_past_the_clock_never_comes() {
        let now = Instant::now();
        assert!(deadline(now, Duration::MAX) > now + Duration::from_secs(86400 * 365));
        assert_eq!(
            deadline(now, Duration::from_secs(1)),
            now + Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn bytes_cross_both_ways_and_half_close_is_passed_on() {
        let (mut c, c_proxy) = duplex(1024);
        let (u_proxy, mut u) = duplex(1024);
        let task = tokio::spawn(async move { relay(c_proxy, u_proxy, &cfg(64)).await });
        c.write_all(b"hello pacer").await.unwrap();
        let mut buf = [0u8; 11];
        u.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello pacer");
        // The client half-closes; the upstream sees EOF and can still answer.
        c.shutdown().await.unwrap();
        let mut rest = Vec::new();
        u.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
        u.write_all(b"reply after eof").await.unwrap();
        u.shutdown().await.unwrap();
        let mut got = Vec::new();
        c.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"reply after eof");
        let out = task.await.unwrap();
        assert_eq!(
            out,
            Outcome {
                reason: CloseReason::Done,
                bytes_up: 11,
                bytes_down: 15
            }
        );
    }

    #[tokio::test]
    async fn the_upstream_may_finish_first() {
        let (mut c, c_proxy) = duplex(1024);
        let (u_proxy, mut u) = duplex(1024);
        let task = tokio::spawn(async move { relay(c_proxy, u_proxy, &cfg(16)).await });
        u.write_all(b"12345").await.unwrap();
        u.shutdown().await.unwrap();
        let mut got = Vec::new();
        c.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"12345");
        // The client can still send after the upstream's end of stream.
        c.write_all(b"late").await.unwrap();
        c.shutdown().await.unwrap();
        let mut up = Vec::new();
        u.read_to_end(&mut up).await.unwrap();
        assert_eq!(up, b"late");
        assert_eq!(task.await.unwrap().reason, CloseReason::Done);
    }

    #[tokio::test(start_paused = true)]
    async fn client_bytes_and_eof_before_the_connect_are_delivered() {
        let (mut c, c_proxy) = duplex(1024);
        let (u_proxy, mut u) = duplex(1024);
        let connect = async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            Ok(u_proxy)
        };
        let task = tokio::spawn(async move { proxy_with(c_proxy, connect, &cfg(32)).await });
        c.write_all(b"early bytes").await.unwrap();
        c.shutdown().await.unwrap();
        let mut got = Vec::new();
        u.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"early bytes");
        u.shutdown().await.unwrap();
        let out = task.await.unwrap();
        assert_eq!(out.reason, CloseReason::Done);
        assert_eq!(out.bytes_up, 11);
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_that_sends_more_than_a_buffer_while_connecting_waits() {
        let (mut c, c_proxy) = duplex(8);
        let (u_proxy, mut u) = duplex(1024);
        let read = Arc::new(Mutex::new(0u64));
        let counted = Counting {
            inner: c_proxy,
            read: read.clone(),
        };
        let connect = async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(u_proxy)
        };
        let task = tokio::spawn(async move { proxy_with(counted, connect, &cfg(16)).await });
        let writer = tokio::spawn(async move {
            c.write_all(&[7u8; 100]).await.unwrap();
            c.shutdown().await.unwrap();
            c
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Before the connect, the proxy holds one buffer and no more.
        assert_eq!(*read.lock().unwrap(), 16);
        let mut got = Vec::new();
        u.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, [7u8; 100]);
        u.shutdown().await.unwrap();
        drop(writer.await.unwrap());
        assert_eq!(task.await.unwrap().bytes_up, 100);
    }

    #[tokio::test]
    async fn a_failed_connect_ends_the_relay_with_its_reason() {
        let (_c, c_proxy) = duplex(64);
        let out = proxy_with(
            c_proxy,
            ready(Err::<tokio::io::DuplexStream, _>(
                CloseReason::UpstreamRefused,
            )),
            &cfg(16),
        )
        .await;
        assert_eq!(
            out,
            Outcome {
                reason: CloseReason::UpstreamRefused,
                bytes_up: 0,
                bytes_down: 0
            }
        );
    }

    #[tokio::test]
    async fn a_client_error_while_connecting_drops_the_connect() {
        let client = Scripted::new(vec![
            Step::Read(b"abc".to_vec()),
            Step::ReadErr(io::ErrorKind::ConnectionReset),
        ]);
        let out = proxy_with(
            client,
            std::future::pending::<Result<Scripted, CloseReason>>(),
            &cfg(16),
        )
        .await;
        assert_eq!(out.reason, CloseReason::ClientReset);
        assert_eq!(out.bytes_up, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn silence_in_both_directions_hits_the_idle_timeout() {
        let (mut c, c_proxy) = duplex(64);
        let (u_proxy, mut u) = duplex(64);
        let idle = Duration::from_secs(10);
        let cfg = RelayConfig::new(16, Duration::from_secs(1), idle).unwrap();
        let start = Instant::now();
        let task = tokio::spawn(async move { relay(c_proxy, u_proxy, &cfg).await });
        // Activity every 6 seconds in alternating directions keeps it open.
        for i in 0..5 {
            tokio::time::sleep(Duration::from_secs(6)).await;
            let mut b = [0u8; 1];
            if i % 2 == 0 {
                c.write_all(b"x").await.unwrap();
                u.read_exact(&mut b).await.unwrap();
            } else {
                u.write_all(b"y").await.unwrap();
                c.read_exact(&mut b).await.unwrap();
            }
        }
        let last = Instant::now();
        let out = task.await.unwrap();
        assert_eq!(out.reason, CloseReason::IdleTimeout);
        assert_eq!((out.bytes_up, out.bytes_down), (3, 2));
        assert_eq!(Instant::now() - last, idle);
        assert_eq!(last - start, Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn a_destination_that_takes_bytes_does_not_count_as_activity() {
        // pacerd refreshes the idle deadline on reads only. The client sends
        // once and the upstream takes it; then neither sends.
        let (mut c, c_proxy) = duplex(64);
        let (u_proxy, mut u) = duplex(64);
        let cfg = RelayConfig::new(16, Duration::from_secs(1), Duration::from_secs(3)).unwrap();
        let task = tokio::spawn(async move { relay(c_proxy, u_proxy, &cfg).await });
        c.write_all(b"hello").await.unwrap();
        let mut b = [0u8; 5];
        u.read_exact(&mut b).await.unwrap();
        let out = task.await.unwrap();
        assert_eq!(out.reason, CloseReason::IdleTimeout);
        assert_eq!(out.bytes_up, 5);
    }

    #[tokio::test]
    async fn a_stalled_destination_stops_the_source_being_read() {
        // The upstream never reads: the proxy reads one buffer plus what
        // the upstream pipe holds, and no more.
        let (mut c, c_proxy) = duplex(4096);
        let (u_proxy, u) = duplex(100);
        let read = Arc::new(Mutex::new(0u64));
        let counted = Counting {
            inner: c_proxy,
            read: read.clone(),
        };
        let task = tokio::spawn(async move { relay(counted, u_proxy, &cfg(256)).await });
        let w = tokio::spawn(async move {
            let _ = c.write_all(&vec![1u8; 1 << 20]).await;
        });
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(*read.lock().unwrap(), 256 + 100);
        drop(u);
        let out = task.await.unwrap();
        assert_eq!(out.reason, CloseReason::UpstreamReset);
        w.abort();
    }

    #[tokio::test]
    async fn read_and_write_errors_name_the_side() {
        type Case = (Vec<Step>, Vec<Step>, CloseReason);
        use io::ErrorKind::*;
        let cases: Vec<Case> = vec![
            (
                vec![Step::ReadErr(ConnectionReset)],
                vec![],
                CloseReason::ClientReset,
            ),
            (
                vec![Step::ReadErr(PermissionDenied)],
                vec![],
                CloseReason::ClientError,
            ),
            (
                vec![],
                vec![Step::ReadErr(ConnectionReset)],
                CloseReason::UpstreamReset,
            ),
            (
                vec![],
                vec![Step::ReadErr(Other)],
                CloseReason::UpstreamError,
            ),
            // Writes to the upstream fail.
            (
                vec![Step::Read(b"up".to_vec())],
                vec![Step::WriteErr(BrokenPipe)],
                CloseReason::UpstreamReset,
            ),
            (
                vec![Step::Read(b"up".to_vec())],
                vec![Step::WriteZero],
                CloseReason::UpstreamError,
            ),
            // Writes to the client fail.
            (
                vec![Step::WriteErr(ConnectionReset)],
                vec![Step::Read(b"down".to_vec())],
                CloseReason::ClientReset,
            ),
            (
                vec![Step::WriteZero],
                vec![Step::Read(b"down".to_vec())],
                CloseReason::ClientError,
            ),
        ];
        for (i, (client, upstream, want)) in cases.into_iter().enumerate() {
            let out = relay(Scripted::new(client), Scripted::new(upstream), &cfg(16)).await;
            assert_eq!(out.reason, want, "case {i}");
        }
    }

    #[tokio::test]
    async fn a_failed_shutdown_still_counts_as_passed_on() {
        let client = Scripted::new(vec![Step::Read(b"x".to_vec()), Step::Eof]);
        let mut upstream = Scripted::new(vec![Step::Eof]);
        upstream.shutdown_err = true;
        let mut client = client;
        client.shutdown_err = true;
        let out = relay(client, upstream, &cfg(16)).await;
        assert_eq!(out.reason, CloseReason::Done);
        assert_eq!(out.bytes_up, 1);
    }

    #[test]
    fn the_buffer_is_moved_back_when_its_tail_runs_out() {
        // A destination that takes 3 bytes at a time and a source that
        // fills the 8-byte buffer: the unwritten bytes move to the front.
        let data: Vec<u8> = (0..64).collect();
        let client = Scripted::new(
            data.chunks(5)
                .map(|c| Step::Read(c.to_vec()))
                .chain([Step::Eof])
                .collect(),
        );
        let mut upstream = Scripted::new(vec![Step::Eof]);
        upstream.write_max = 3;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let (out, written) = rt.block_on(async {
            let sink = upstream.written.clone();
            let out = relay(client, upstream, &cfg(8)).await;
            let w = sink.lock().unwrap().clone();
            (out, w)
        });
        assert_eq!(out.reason, CloseReason::Done);
        assert_eq!(written, data);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Any interleaving of short reads, short writes and pending polls
        /// on both sides delivers both byte streams intact and in order,
        /// with each end of stream after its data.
        #[test]
        fn streams_arrive_intact_under_any_interleaving(
            up in proptest::collection::vec(any::<u8>(), 0..600),
            down in proptest::collection::vec(any::<u8>(), 0..600),
            up_chunk in 1usize..40,
            down_chunk in 1usize..40,
            write_max in 1usize..17,
            pend_every in 2u32..6,
            buffer in 1usize..48,
        ) {
            let script = |data: &[u8], chunk: usize| -> Vec<Step> {
                let mut s = Vec::new();
                for (i, c) in data.chunks(chunk).enumerate() {
                    if i as u32 % pend_every == 1 {
                        s.push(Step::Pending);
                    }
                    s.push(Step::Read(c.to_vec()));
                }
                s.push(Step::Pending);
                s.push(Step::Eof);
                s
            };
            let mut client = Scripted::new(script(&up, up_chunk));
            let mut upstream = Scripted::new(script(&down, down_chunk));
            client.write_max = write_max;
            upstream.write_max = write_max + 3;
            client.pend_writes = pend_every;
            upstream.pend_writes = pend_every + 1;
            let to_client = client.written.clone();
            let to_upstream = upstream.written.clone();
            let shut_c = client.shut.clone();
            let shut_u = upstream.shut.clone();
            let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
            let out = rt.block_on(relay(client, upstream, &cfg(buffer)));
            prop_assert_eq!(out.reason, CloseReason::Done);
            prop_assert_eq!(out.bytes_up, up.len() as u64);
            prop_assert_eq!(out.bytes_down, down.len() as u64);
            prop_assert_eq!(&*to_upstream.lock().unwrap(), &up);
            prop_assert_eq!(&*to_client.lock().unwrap(), &down);
            prop_assert_eq!(*shut_c.lock().unwrap(), Some(down.len()));
            prop_assert_eq!(*shut_u.lock().unwrap(), Some(up.len()));
        }
    }

    /// Counts the bytes read through it.
    struct Counting<T> {
        inner: T,
        read: Arc<Mutex<u64>>,
    }

    impl<T: AsyncRead + Unpin> AsyncRead for Counting<T> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let before = buf.filled().len();
            let r = Pin::new(&mut self.inner).poll_read(cx, buf);
            *self.read.lock().unwrap() += (buf.filled().len() - before) as u64;
            r
        }
    }

    impl<T: AsyncWrite + Unpin> AsyncWrite for Counting<T> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    /// One step of a scripted stream's read side.
    #[derive(Debug, Clone)]
    enum Step {
        /// Hand out these bytes (as many as fit; the rest stay).
        Read(Vec<u8>),
        /// Return pending once, waking at once.
        Pending,
        /// Fail the next read.
        ReadErr(io::ErrorKind),
        /// Fail the next write.
        WriteErr(io::ErrorKind),
        /// Return Ok(0) from the next write.
        WriteZero,
        /// End of stream.
        Eof,
    }

    /// A stream whose reads follow a script and whose writes are recorded.
    /// Once the script runs out, reads stay pending forever.
    struct Scripted {
        steps: VecDeque<Step>,
        written: Arc<Mutex<Vec<u8>>>,
        /// Bytes written when shutdown was called.
        shut: Arc<Mutex<Option<usize>>>,
        write_max: usize,
        /// Every n-th write returns pending first (0: never).
        pend_writes: u32,
        writes: u32,
        shutdown_err: bool,
    }

    impl Scripted {
        fn new(steps: Vec<Step>) -> Self {
            Scripted {
                steps: steps.into(),
                written: Arc::default(),
                shut: Arc::default(),
                write_max: usize::MAX,
                pend_writes: 0,
                writes: 0,
                shutdown_err: false,
            }
        }
    }

    impl AsyncRead for Scripted {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.steps.front_mut() {
                None => Poll::Pending,
                Some(Step::Read(data)) => {
                    let n = data.len().min(buf.remaining());
                    buf.put_slice(&data[..n]);
                    data.drain(..n);
                    if data.is_empty() {
                        self.steps.pop_front();
                    }
                    Poll::Ready(Ok(()))
                }
                Some(Step::Pending) => {
                    self.steps.pop_front();
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                Some(Step::ReadErr(kind)) => {
                    let kind = *kind;
                    self.steps.pop_front();
                    Poll::Ready(Err(kind.into()))
                }
                Some(Step::Eof) => Poll::Ready(Ok(())),
                // Write steps wait for a write; reads before them pend.
                Some(Step::WriteErr(_) | Step::WriteZero) => Poll::Pending,
            }
        }
    }

    impl AsyncWrite for Scripted {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match self.steps.front() {
                Some(Step::WriteErr(kind)) => {
                    let kind = *kind;
                    self.steps.pop_front();
                    return Poll::Ready(Err(kind.into()));
                }
                Some(Step::WriteZero) => {
                    self.steps.pop_front();
                    return Poll::Ready(Ok(0));
                }
                _ => {}
            }
            self.writes += 1;
            if self.pend_writes > 0 && self.writes.is_multiple_of(self.pend_writes) {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            let n = buf.len().min(self.write_max);
            self.written.lock().unwrap().extend_from_slice(&buf[..n]);
            Poll::Ready(Ok(n))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            let n = self.written.lock().unwrap().len();
            *self.shut.lock().unwrap() = Some(n);
            if self.shutdown_err {
                return Poll::Ready(Err(io::ErrorKind::NotConnected.into()));
            }
            Poll::Ready(Ok(()))
        }
    }
}
