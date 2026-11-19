//! One accepted connection: admission, the relay with byte shaping and the
//! slow-client check on its data path, and the close accounting. The
//! parts of pacerd's `pd_conn_accept`, `pump`, `pd_conn_timer` and
//! `pd_conn_close` (`proxy.c`) that edgefront_core::l4 does not cover.

use std::net::SocketAddr;
use std::sync::Arc;

use edgefront_core::l4::{self, CloseReason, Direction, Grant, Pace, RelayConfig};
use edgefront_limiter::conn::Permit;
use edgefront_limiter::shape::Dir;
use edgefront_limiter::slowloris::Check;
use edgefront_limiter::{LogLevel, Refusal, Shaper, SlowCheck, Stat};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::time::Instant;

use super::daemon::{Clock, Listener};

/// The [`Pace`] of one connection: its listener's byte limits and its own
/// slow-client check, on the daemon's clock.
pub struct ConnPace {
    clock: Clock,
    listener: Arc<Listener>,
    shaper: Option<Shaper>,
    slow: Option<SlowCheck>,
}

impl ConnPace {
    pub fn new(clock: Clock, listener: Arc<Listener>, permit: &Permit, now_ns: u64) -> Self {
        let shaper = Shaper::new(Arc::clone(&listener.bandwidth), permit.key());
        let slow = listener
            .config()
            .min_rate
            .and_then(|m| SlowCheck::new(&m, now_ns));
        Self {
            clock,
            listener,
            shaper: Some(shaper),
            slow,
        }
    }
}

impl Pace for ConnPace {
    fn reserve(&mut self, dir: Direction, want: usize) -> Grant {
        let Some(shaper) = self.shaper.as_mut().filter(|s| s.is_on()) else {
            return Grant::Read(want);
        };
        let now = self.clock.now_ns();
        let dir = match dir {
            Direction::Up => Dir::Up,
            Direction::Down => Dir::Down,
        };
        let r = shaper.reserve(dir, want as u64, now);
        if r.granted > 0 {
            // A grant is at most `want`, a usize.
            return Grant::Read(r.granted as usize);
        }
        let until = now.saturating_add(r.retry_ns.max(1));
        if let Some(s) = &mut self.slow {
            s.throttled(until);
        }
        self.listener.admission.counters().add(Stat::Throttled, 1);
        Grant::Wait(self.clock.instant(until))
    }

    fn give_back(&mut self, _dir: Direction, unused: usize) {
        if unused == 0 {
            return;
        }
        if let Some(s) = self.shaper.as_ref().filter(|s| s.is_on()) {
            s.give_back(unused as u64, self.clock.now_ns());
        }
    }

    fn moved(&mut self, dir: Direction, n: usize) {
        let stat = match dir {
            Direction::Up => Stat::BytesUp,
            Direction::Down => Stat::BytesDown,
        };
        self.listener.admission.counters().add(stat, n as u64);
        if let Some(s) = &mut self.slow {
            s.moved(self.clock.now_ns(), n as u64);
        }
    }

    fn deadline(&self) -> Option<Instant> {
        self.slow
            .as_ref()
            .map(|s| self.clock.instant(s.next_check()))
    }

    fn check(&mut self) -> Result<(), CloseReason> {
        let Some(s) = &mut self.slow else {
            return Ok(());
        };
        let min = self.listener.config().min_rate;
        match s.check(self.clock.now_ns(), min.as_ref()) {
            Check::Pass { .. } => Ok(()),
            Check::Cut => Err(CloseReason::SlowClient),
            Check::Off => {
                self.slow = None;
                Ok(())
            }
        }
    }
}

impl Drop for ConnPace {
    /// A closed connection gives back what it booked in the listener's
    /// bucket and did not read.
    fn drop(&mut self) {
        if let Some(s) = &mut self.shaper {
            s.release(self.clock.now_ns());
        }
    }
}

/// Everything a connection task needs from the daemon.
pub struct Context {
    pub clock: Clock,
    pub log: Arc<super::log::Log>,
    /// Becomes true when the drain timeout runs out.
    pub stop: watch::Receiver<bool>,
}

/// What a connection is closed with when the drain ends it, as the
/// relay's stop future gives it.
async fn stopped(mut stop: watch::Receiver<bool>) -> CloseReason {
    // A dropped sender means the daemon is gone: stop too.
    let _ = stop.wait_for(|s| *s).await;
    CloseReason::DrainTimeout
}

/// Admits `stream` from `peer` on `listener`, or refuses it with nothing
/// read, logging the refusal at most as often as pacerd's refusal limit
/// allows. Returns the permit for an admitted connection.
pub fn admit(
    ctx: &Context,
    listener: &Arc<Listener>,
    peer: SocketAddr,
    now_ns: u64,
) -> Result<Permit, Refusal> {
    match listener.admission.admit(peer.ip(), now_ns) {
        Ok(p) => Ok(p),
        Err(r) => {
            log_refusal(ctx, listener, &peer.to_string(), r, now_ns);
            Err(r)
        }
    }
}

/// pacerd's `pd_log_reject`: `conn rejected` with the listener, peer and
/// reason, and `similar_dropped` when the limit held lines back.
pub fn log_refusal(ctx: &Context, listener: &Listener, peer: &str, r: Refusal, now_ns: u64) {
    let Some(dropped) = listener.reject_log.allow(now_ns) else {
        return;
    };
    let mut line = ctx
        .log
        .line(LogLevel::Info, "conn rejected")
        .str("listener", listener.name())
        .str("peer", peer)
        .str("reason", r.as_str());
    if dropped > 0 {
        line = line.u64("similar_dropped", dropped);
    }
    drop(line);
}

/// Runs one admitted connection to its end: connects to the listener's
/// upstream as configured now, relays with the connection's pace, counts
/// the close, gives back the permit and only then logs `conn closed`, so a
/// client that reads the line and reconnects at once is admitted.
pub async fn serve(
    ctx: Context,
    listener: Arc<Listener>,
    stream: TcpStream,
    peer: SocketAddr,
    permit: Permit,
    now_ns: u64,
) {
    let id = ctx.log.next_conn_id();
    let cfg = listener.config();
    let peer_s = peer.to_string();
    drop(
        ctx.log
            .line(LogLevel::Debug, "conn open")
            .u64("conn", id)
            .str("listener", &cfg.name)
            .str("peer", &peer_s),
    );
    // The config reader holds these to pacerd's bounds, so none is zero.
    let relay = RelayConfig::new(
        cfg.buffer_size as usize,
        cfg.connect_timeout,
        cfg.idle_timeout,
    )
    .unwrap_or_default();
    let _ = stream.set_nodelay(true);
    let pace = ConnPace::new(ctx.clock, Arc::clone(&listener), &permit, now_ns);
    let started = std::time::Instant::now();
    let out = l4::proxy_paced(
        stream,
        l4::connect(cfg.upstream, relay.connect_timeout()),
        &relay,
        pace,
        stopped(ctx.stop.clone()),
    )
    .await;
    listener.admission.counters().count(out.reason.as_str());
    drop(permit);
    let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    drop(
        ctx.log
            .line(LogLevel::Info, "conn closed")
            .u64("conn", id)
            .str("listener", &cfg.name)
            .str("peer", &peer_s)
            .str("reason", out.reason.as_str())
            .u64("bytes_up", out.bytes_up)
            .u64("bytes_down", out.bytes_down)
            .u64("ms", ms),
    );
}
