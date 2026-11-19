//! pacerd's log lines (`src/pacerd/log.c`): one logfmt line per event on
//! stderr, `ts=<UTC, ms> level=<level> msg=<msg>` and then `key=value`
//! fields. A value is written bare unless it is empty or has a space,
//! a control or non-ASCII byte, `"`, `=` or `\`, and then quoted with
//! `\"`, `\\`, `\n`, `\t`, `\r` and `\xNN` escapes. A line is cut at 1 KiB
//! and marked with ` ...`. Scripts and tests that read pacerd's log read
//! this one the same way.

use std::io::Write;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use edgefront_limiter::LogLevel;
use edgefront_limiter::gcra::{Gcra, Rate};

/// Bytes of a line before the cut, as pacerd's 1 KiB line buffer minus
/// the room it kept for the marker and the newline.
const LINE_MAX: usize = 1024 - 5;

/// The levels in order.
const LEVELS: [&str; 4] = ["debug", "info", "warn", "error"];

fn rank(l: LogLevel) -> u8 {
    match l {
        LogLevel::Debug => 0,
        LogLevel::Info => 1,
        LogLevel::Warn => 2,
        LogLevel::Error => 3,
    }
}

/// The process's log: the level below which lines are dropped, and the
/// connection ids.
#[derive(Debug)]
pub struct Log {
    min: AtomicU8,
    conn_ids: AtomicU64,
}

impl Log {
    pub fn new(level: LogLevel) -> Self {
        Self {
            min: AtomicU8::new(rank(level)),
            conn_ids: AtomicU64::new(0),
        }
    }

    pub fn set_level(&self, level: LogLevel) {
        self.min.store(rank(level), Ordering::Relaxed);
    }

    /// The next connection id, from 1.
    pub fn next_conn_id(&self) -> u64 {
        self.conn_ids.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Starts a line at `level` with `msg`; it is written when dropped. A
    /// line below the configured level does nothing.
    pub fn line(&self, level: LogLevel, msg: &str) -> Line {
        let skip = rank(level) < self.min.load(Ordering::Relaxed);
        let mut l = Line {
            buf: String::new(),
            skip,
        };
        if !skip {
            l.buf.push_str("ts=");
            l.buf.push_str(&timestamp(SystemTime::now()));
            l.buf.push_str(" level=");
            l.buf.push_str(LEVELS[rank(level) as usize]);
            l.buf.push_str(" msg=");
            put_value(&mut l.buf, msg);
        }
        l
    }

    /// A line with nothing but `msg`.
    pub fn msg(&self, level: LogLevel, msg: &str) {
        drop(self.line(level, msg));
    }
}

/// One log line being built. Written to stderr in one write when dropped.
#[derive(Debug)]
pub struct Line {
    buf: String,
    skip: bool,
}

impl Line {
    pub fn str(mut self, key: &str, value: &str) -> Self {
        if !self.skip {
            self.buf.push(' ');
            self.buf.push_str(key);
            self.buf.push('=');
            put_value(&mut self.buf, value);
        }
        self
    }

    pub fn u64(mut self, key: &str, value: u64) -> Self {
        if !self.skip {
            self.buf.push(' ');
            self.buf.push_str(key);
            self.buf.push('=');
            self.buf.push_str(&value.to_string());
        }
        self
    }

    /// The finished line, without its newline: the first [`LINE_MAX`]
    /// bytes, then ` ...` if anything was cut.
    fn finish(&mut self) -> Vec<u8> {
        let mut out = std::mem::take(&mut self.buf).into_bytes();
        if out.len() > LINE_MAX {
            out.truncate(LINE_MAX);
            out.extend_from_slice(b" ...");
        }
        out.push(b'\n');
        out
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        if self.skip {
            return;
        }
        let out = self.finish();
        // Nowhere is left to report a failed log write; the line is lost,
        // as in pacerd.
        let _ = std::io::stderr().lock().write_all(&out);
    }
}

fn needs_quotes(v: &str) -> bool {
    v.is_empty()
        || v.bytes()
            .any(|c| c <= b' ' || c >= 0x7f || c == b'"' || c == b'=' || c == b'\\')
}

fn put_value(buf: &mut String, v: &str) {
    if !needs_quotes(v) {
        buf.push_str(v);
        return;
    }
    buf.push('"');
    for c in v.bytes() {
        match c {
            b'"' => buf.push_str("\\\""),
            b'\\' => buf.push_str("\\\\"),
            b'\n' => buf.push_str("\\n"),
            b'\t' => buf.push_str("\\t"),
            b'\r' => buf.push_str("\\r"),
            c if !(0x20..0x7f).contains(&c) => buf.push_str(&format!("\\x{c:02x}")),
            c => buf.push(c as char),
        }
    }
    buf.push('"');
}

/// `2026-11-20T06:01:02.345Z` for `t`.
fn timestamp(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, day) = civil(days as i64);
    format!(
        "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_millis()
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01 (Howard
/// Hinnant's `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// A limit on how many lines of one kind are written: pacerd's
/// `pd_log_limit`, 10 a second with a burst of 20 for refusals, and a
/// count of what it held back, reported on the next line it lets through.
#[derive(Debug)]
pub struct Limit {
    gcra: Gcra,
    suppressed: AtomicU64,
}

impl Limit {
    pub fn new(per_sec: u64, burst: u64) -> Self {
        let rate = Rate::new(per_sec, 1_000_000_000, burst);
        Self {
            gcra: Gcra::new(&rate).expect("a constant, valid rate"),
            suppressed: AtomicU64::new(0),
        }
    }

    /// Whether a line may be written at `now_ns`, and if so how many were
    /// held back since the last one.
    pub fn allow(&self, now_ns: u64) -> Option<u64> {
        match self.gcra.take(now_ns, 1) {
            Ok(d) if d.allowed => Some(self.suppressed.swap(0, Ordering::Relaxed)),
            _ => {
                self.suppressed.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn value(v: &str) -> String {
        let mut s = String::new();
        put_value(&mut s, v);
        s
    }

    #[test]
    fn values_are_quoted_as_pacerd_quotes_them() {
        assert_eq!(value("done"), "done");
        assert_eq!(value("127.0.0.1:80"), "127.0.0.1:80");
        assert_eq!(value(""), "\"\"");
        assert_eq!(value("drain timeout"), "\"drain timeout\"");
        assert_eq!(value("a=b"), "\"a=b\"");
        assert_eq!(value("q\"\\"), "\"q\\\"\\\\\"");
        assert_eq!(value("l\n\t\r"), "\"l\\n\\t\\r\"");
        assert_eq!(value("\u{1}"), "\"\\x01\"");
        assert_eq!(value("é"), "\"\\xc3\\xa9\"");
        assert_eq!(value("\u{7f}"), "\"\\x7f\"");
    }

    #[test]
    fn a_line_has_pacerds_shape() {
        let log = Log::new(LogLevel::Info);
        let mut l = log
            .line(LogLevel::Info, "conn closed")
            .u64("conn", 7)
            .str("reason", "done");
        let out = String::from_utf8(l.finish()).unwrap();
        l.skip = true;
        let (ts, rest) = out.split_once(' ').unwrap();
        assert_eq!(ts.len(), "ts=2026-11-20T06:01:02.345Z".len());
        assert!(ts.starts_with("ts=20") && ts.ends_with('Z'));
        assert_eq!(rest, "level=info msg=\"conn closed\" conn=7 reason=done\n");
    }

    #[test]
    fn lines_below_the_level_are_dropped() {
        let log = Log::new(LogLevel::Warn);
        let l = log.line(LogLevel::Info, "x").u64("n", 1).str("k", "v");
        assert!(l.skip && l.buf.is_empty());
        log.set_level(LogLevel::Debug);
        let mut l = log.line(LogLevel::Debug, "x");
        assert!(!l.skip);
        assert!(
            String::from_utf8(l.finish())
                .unwrap()
                .contains("level=debug")
        );
        l.skip = true;
        assert_eq!(log.next_conn_id(), 1);
        assert_eq!(log.next_conn_id(), 2);
        log.msg(LogLevel::Error, "written to stderr");
    }

    #[test]
    fn a_long_line_is_cut_and_marked() {
        let log = Log::new(LogLevel::Info);
        let mut l = log.line(LogLevel::Info, "x").str("v", &"y".repeat(5000));
        let out = l.finish();
        l.skip = true;
        assert_eq!(out.len(), LINE_MAX + 5);
        assert!(out.ends_with(b"y ...\n"));
    }

    #[test]
    fn timestamps_are_utc_with_milliseconds() {
        let t = |s: u64, ms: u64| {
            timestamp(UNIX_EPOCH + Duration::from_secs(s) + Duration::from_millis(ms))
        };
        assert_eq!(t(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(t(951_782_400, 5), "2000-02-29T00:00:00.005Z");
        assert_eq!(t(1_795_154_462, 999), "2026-11-20T06:01:02.999Z");
        assert_eq!(t(4_107_542_399, 0), "2100-02-28T23:59:59.000Z");
        assert_eq!(
            timestamp(UNIX_EPOCH - Duration::from_secs(1)),
            "1970-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn the_limit_lets_a_burst_through_and_counts_the_rest() {
        let lim = Limit::new(10, 20);
        let s = 1_000_000_000;
        for _ in 0..20 {
            assert_eq!(lim.allow(s), Some(0));
        }
        assert_eq!(lim.allow(s), None);
        assert_eq!(lim.allow(s), None);
        // A tenth of a second later one more goes, reporting the two.
        assert_eq!(lim.allow(s + 100_000_000), Some(2));
        assert_eq!(lim.allow(s + 100_000_000), None);
    }
}
