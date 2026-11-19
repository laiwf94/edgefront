//! The admin API's reports, from `edgefront_limiter::stats`: the `stats`
//! answer in pacerd's `pd_stats_render` text (a header line, then one line
//! per listener), and the client listings pacerctl did not have. pacerd's
//! per-worker `accepted` lines have no counterpart: connections are not
//! tied to a worker thread here.

use std::fmt::Write;
use std::net::IpAddr;

use edgefront_limiter::stats::{self, ListenerStats};

use super::daemon::{Daemon, Listener};

/// The stats text for the daemon as it is now.
pub fn render(d: &Daemon) -> String {
    let cfg = d.config();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "edgefront version={} uptime_s={} workers={} draining={}",
        env!("CARGO_PKG_VERSION"),
        d.clock.now_ns() / 1_000_000_000,
        cfg.workers,
        u8::from(d.is_draining())
    );
    for l in &d.listeners {
        let _ = writeln!(out, "{}", ListenerStats::of(&l.admission, &l.bandwidth));
    }
    out
}

/// The listeners a request names: the one called `name`, or every one.
fn pick<'a>(d: &'a Daemon, name: Option<&str>) -> Result<Vec<&'a Listener>, String> {
    let all: Vec<&Listener> = d.listeners.iter().map(|l| &**l).collect();
    match name {
        None => Ok(all),
        Some(n) => {
            let found: Vec<&Listener> = all.into_iter().filter(|l| l.name() == n).collect();
            if found.is_empty() {
                Err(format!("no listener {n}"))
            } else {
                Ok(found)
            }
        }
    }
}

/// The `top` busiest clients of each listener (or of the one named).
pub fn top(d: &Daemon, listener: Option<&str>, top: usize) -> Result<String, String> {
    let now = d.clock.now_ns();
    Ok(pick(d, listener)?
        .into_iter()
        .map(|l| stats::render_top(&l.admission, &l.bandwidth, top, now))
        .collect())
}

/// One address's limits on each listener (or on the one named).
pub fn client(d: &Daemon, listener: Option<&str>, addr: &str) -> Result<String, String> {
    let ip: IpAddr = addr
        .parse()
        .map_err(|_| format!("not an IP address: {addr}"))?;
    let now = d.clock.now_ns();
    let mut out = String::new();
    for l in pick(d, listener)? {
        let c = stats::client(&l.admission, &l.bandwidth, ip, now);
        let _ = writeln!(out, "listener={} {c}", l.name());
    }
    Ok(out)
}
