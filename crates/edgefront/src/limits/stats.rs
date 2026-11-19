//! The `stats` answer: pacerd's `pd_stats_render` text, one header line
//! and one line per listener with its open count, its counters in
//! pacerd's order (then edgefront's own `rejected_per_ip_quota`) and its
//! client table's size. pacerd's per-worker `accepted` lines have no
//! counterpart: connections are not tied to a worker thread here.

use std::fmt::Write;

use super::daemon::Daemon;

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
        let a = &l.admission;
        let _ = write!(out, "listener={} open={}", a.name(), a.open());
        for (name, v) in a.counters().snapshot() {
            let _ = write!(out, " {name}={v}");
        }
        let t = a.client_stats();
        let _ = writeln!(
            out,
            " clients_tracked={} client_table_capacity={} client_table_evictions={}",
            t.entries, t.capacity, t.evictions
        );
    }
    out
}
