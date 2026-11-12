//! The limiter's configuration, ported from pacerd's `config.c`.
//!
//! [`LimiterConfig`] is the typed form: worker count, admin socket, drain
//! timeout and log level, plus up to sixteen [`ListenerConfig`]s, each an
//! L4 listener with its upstream, connection limits, timeouts, slow-client
//! cut-off and rate limits. It reaches it two ways:
//!
//! - [`parse_pacerd`] reads pacerd.conf as pacerd did. Every file pacerd
//!   accepts gives the same values, and every file it rejects is rejected
//!   with the same message and line, such as
//!   `line 12: per_ip_conn_rate: expected a rate like 10/s, 600/m or 1m/s, got "10/d"`.
//! - serde, as a section of an edgefront config. Field names are the
//!   pacerd directive names, each value is the pacerd value as a string
//!   (`"5s"`, `"16k"`, `"10/s burst 20"`, `"512 per 10s"`) or an integer
//!   for counts and sizes, and listeners are a `listeners` array of tables
//!   with a `name`. Values go through the same parsers and checks, so the
//!   messages are pacerd's with the field's path in front:
//!   `listeners[0].max_conns: must be between 1 and 1000000`.
//!
//! Both readers share one directive table, so a value means the same in
//! either syntax. [`LimiterConfig::to_pacerd`] and the `Serialize`
//! implementation write a config back out, and the result reads back into
//! an equal config.

use std::fmt;
use std::io::Read;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::{self, Deserializer, Visitor};
use serde::ser::{self, Serializer};
use serde::{Deserialize, Serialize};

use crate::gcra::Rate;
use crate::table::FullPolicy;

/// The most listeners one config may define.
pub const MAX_LISTENERS: usize = 16;
/// A listener name is 1 to `NAME_MAX - 1` bytes of letters, digits, `_`
/// and `-`.
pub const NAME_MAX: usize = 32;
/// The admin socket path must be shorter than this (`sun_path`).
pub const ADMIN_PATH_MAX: usize = 108;
/// The largest pacerd.conf, in bytes.
pub const MAX_FILE_BYTES: usize = 1 << 20;
/// The longest line in a pacerd.conf, in bytes, without its newline.
pub const LINE_MAX: usize = 1024;

/// Words on one line, the directive included.
const MAX_TOKENS: usize = 8;
const SEC: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;
/// The largest size or token count a value may give.
const MAX_SIZE: u64 = 1 << 50;
/// The largest duration a value may give, before the directive's own range.
const MAX_DUR: u64 = 86_400 * SEC;
/// A number in a size or duration has fewer digits than this.
const NUM_DIGITS_MAX: usize = 24;
/// The part of a rate before its `/` is shorter than this.
const RATE_NUM_MAX: usize = 32;
/// Tokens quoted in messages are cut to this many bytes.
const SHOWN_MAX: usize = 44;

/// How much pacerd logs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LogLevel {
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

impl LogLevel {
    const ALL: [(LogLevel, &'static str); 4] = [
        (LogLevel::Debug, "debug"),
        (LogLevel::Info, "info"),
        (LogLevel::Warn, "warn"),
        (LogLevel::Error, "error"),
    ];

    /// The name a config uses for this level.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

/// The slow-client cut-off: a connection that moves fewer than `bytes` in
/// `window` is closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MinRate {
    pub bytes: u64,
    pub window: Duration,
}

/// One L4 listener and its limits. [`ListenerConfig::new`] gives pacerd's
/// defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenerConfig {
    pub name: String,
    pub listen: SocketAddr,
    pub upstream: SocketAddr,
    /// Open connections at once across the listener.
    pub max_conns: u32,
    /// Bytes of buffer per direction per connection.
    pub buffer_size: u32,
    pub connect_timeout: Duration,
    pub idle_timeout: Duration,
    /// `None` turns the slow-client check off.
    pub min_rate: Option<MinRate>,
    /// Open connections per client address; 0 means no cap.
    pub per_ip_max_conns: u32,
    /// IPv6 clients are grouped by this prefix length.
    pub ipv6_prefix: u32,
    /// New connections per client address.
    pub per_ip_conn_rate: Option<Rate>,
    /// Bytes per second per client address.
    pub per_ip_bandwidth: Option<Rate>,
    /// Bytes per second across the listener.
    pub bandwidth: Option<Rate>,
    /// Client addresses tracked at once.
    pub table_size: u32,
    pub table_full: FullPolicy,
}

impl ListenerConfig {
    /// A listener with pacerd's defaults for everything but its name and
    /// addresses.
    pub fn new(name: impl Into<String>, listen: SocketAddr, upstream: SocketAddr) -> Self {
        Self {
            name: name.into(),
            listen,
            upstream,
            max_conns: 1024,
            buffer_size: 16 * 1024,
            connect_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(300),
            min_rate: None,
            per_ip_max_conns: 0,
            ipv6_prefix: 64,
            per_ip_conn_rate: None,
            per_ip_bandwidth: None,
            bandwidth: None,
            table_size: 16_384,
            table_full: FullPolicy::Deny,
        }
    }
}

/// The limiter section: process-wide settings and the listeners.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimiterConfig {
    /// Worker threads, 1 to 64.
    pub workers: u32,
    /// `None`: no admin socket.
    pub admin_socket: Option<PathBuf>,
    pub drain_timeout: Duration,
    pub log_level: LogLevel,
    pub listeners: Vec<ListenerConfig>,
}

impl Default for LimiterConfig {
    /// pacerd's defaults, with no listeners. A config needs at least one to
    /// be valid.
    fn default() -> Self {
        Self {
            workers: 1,
            admin_socket: None,
            drain_timeout: Duration::from_secs(30),
            log_level: LogLevel::Info,
            listeners: Vec::new(),
        }
    }
}

/// Why a pacerd.conf was rejected: the 1-based line and pacerd's message.
/// Errors found at the end of the file name its last line, and a file too
/// large to read names line 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ConfigError {}

/// The 1-based line each directive of one scope was set on. A directive
/// left at its default has no line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lines {
    lines: Vec<(&'static str, usize)>,
}

impl Lines {
    /// Records that `directive` was set on `line`. `directive` must be one
    /// of the directive names; others are ignored.
    pub fn set(&mut self, directive: &str, line: usize) {
        let Some((_, d)) = directive_named(directive) else {
            return;
        };
        match self.lines.iter_mut().find(|(n, _)| *n == d.name) {
            Some(slot) => slot.1 = line,
            None => self.lines.push((d.name, line)),
        }
    }

    /// The line `directive` was set on, if it was set.
    pub fn get(&self, directive: &str) -> Option<usize> {
        self.lines
            .iter()
            .find(|(n, _)| *n == directive)
            .map(|&(_, l)| l)
    }
}

/// One listener's lines: its block's opening line and its directives.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListenerLines {
    /// The line that opens the listener (`listener NAME {`, or the
    /// `[[listeners]]` header).
    pub line: usize,
    pub directives: Lines,
}

impl ListenerLines {
    pub fn new(line: usize) -> Self {
        Self {
            line,
            directives: Lines::default(),
        }
    }

    pub fn set(&mut self, directive: &str, line: usize) {
        self.directives.set(directive, line);
    }

    /// The directive's line, or the listener's own line when the directive
    /// was left at its default.
    pub fn line_of(&self, directive: &str) -> usize {
        self.directives.get(directive).unwrap_or(self.line)
    }
}

/// Where each value of a config came from, so that [`crate::lint`] can
/// name a line. [`parse_pacerd_mapped`] fills it from pacerd.conf; a TOML
/// reader fills it from its spans. `listeners[i]` belongs to the config's
/// `listeners[i]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceMap {
    pub top: Lines,
    pub listeners: Vec<ListenerLines>,
}

/// The names of every directive, top-level ones first, in config.c's
/// order.
pub fn directive_names() -> impl Iterator<Item = &'static str> {
    DIRECTIVES.iter().map(|d| d.name)
}

fn directive_named(name: &str) -> Option<(usize, &'static Directive)> {
    directive(name.as_bytes())
}

// ---------------------------------------------------------------------------
// Value parsers, one for one with config.c.

fn parse_u64(s: &[u8]) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    s.iter().try_fold(0u64, |v, &c| {
        if !c.is_ascii_digit() {
            return None;
        }
        v.checked_mul(10)?.checked_add(u64::from(c - b'0'))
    })
}

/// Splits leading digits from a unit suffix: `16k` gives `16` and `k`.
fn split_unit(s: &[u8]) -> Option<(&[u8], &[u8])> {
    let digits = s.iter().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits >= NUM_DIGITS_MAX {
        return None;
    }
    Some(s.split_at(digits))
}

fn scaled(v: u64, mul: u64, max: u64) -> Option<u64> {
    v.checked_mul(mul).filter(|&n| n <= max)
}

/// A byte size: digits with an optional `k`, `m` or `g` (powers of 1024).
fn parse_size(s: &[u8]) -> Option<u64> {
    let (num, unit) = split_unit(s)?;
    let v = parse_u64(num)?;
    let mul = match unit {
        b"" => 1,
        b"k" => 1 << 10,
        b"m" => 1 << 20,
        b"g" => 1 << 30,
        _ => return None,
    };
    scaled(v, mul, MAX_SIZE)
}

/// A duration in nanoseconds: digits with a required `ms`, `s`, `m` or `h`.
fn parse_duration(s: &[u8]) -> Option<u64> {
    let (num, unit) = split_unit(s)?;
    let v = parse_u64(num)?;
    let mul = match unit {
        b"ms" => MS,
        b"s" => SEC,
        b"m" => 60 * SEC,
        b"h" => 3600 * SEC,
        _ => return None,
    };
    scaled(v, mul, MAX_DUR)
}

/// `10/s`, `1m/s` (sizes allowed), `600/m`, `100/h`. The burst is the
/// token count.
fn parse_rate(s: &[u8]) -> Option<Rate> {
    let slash = s.iter().position(|&c| c == b'/')?;
    if slash >= RATE_NUM_MAX {
        return None;
    }
    let tokens = parse_size(&s[..slash]).filter(|&t| t != 0)?;
    let period_ns = match &s[slash + 1..] {
        b"s" => SEC,
        b"m" => 60 * SEC,
        b"h" => 3600 * SEC,
        _ => return None,
    };
    Some(Rate::new(tokens, period_ns, tokens))
}

/// `1.2.3.4:80` or `[::1]:80`, numeric only, port 1 to 65535.
pub fn parse_addr(s: &[u8]) -> Option<SocketAddr> {
    const HOST_MAX: usize = 64;
    let bracketed = s.first() == Some(&b'[');
    let (host, port) = if bracketed {
        let end = s.iter().position(|&c| c == b']')?;
        if s.get(end + 1) != Some(&b':') || end > HOST_MAX {
            return None;
        }
        (&s[1..end], &s[end + 2..])
    } else {
        let colon = s.iter().rposition(|&c| c == b':')?;
        if colon >= HOST_MAX {
            return None;
        }
        (&s[..colon], &s[colon + 1..])
    };
    let port = parse_u64(port)
        .filter(|p| (1..=65_535).contains(p))
        .and_then(|p| u16::try_from(p).ok())?;
    let host = std::str::from_utf8(host).ok()?;
    if bracketed {
        Some(SocketAddr::new(host.parse::<Ipv6Addr>().ok()?.into(), port))
    } else {
        Some(SocketAddr::new(host.parse::<Ipv4Addr>().ok()?.into(), port))
    }
}

/// Quotes a token in a message: printable ASCII but `"` kept, anything else
/// shown as `?`, and more than 44 bytes cut short with `...`, so a hostile
/// file cannot put control bytes into a log.
fn shown(tok: &[u8]) -> String {
    let mut out: String = tok
        .iter()
        .take(SHOWN_MAX)
        .map(|&c| {
            if (0x20..0x7f).contains(&c) && c != b'"' {
                char::from(c)
            } else {
                '?'
            }
        })
        .collect();
    if tok.len() > SHOWN_MAX {
        out.push_str("...");
    }
    out
}

fn valid_name(s: &[u8]) -> bool {
    !s.is_empty()
        && s.len() < NAME_MAX
        && s.iter()
            .all(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn name_error(name: &[u8]) -> String {
    format!(
        "listener name \"{}\" must be 1-31 letters, digits, _ or -",
        shown(name)
    )
}

// ---------------------------------------------------------------------------
// The directive table, shared by both readers.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Workers,
    AdminSocket,
    DrainTimeout,
    LogLevel,
    Listen,
    Upstream,
    MaxConns,
    Buffer,
    ConnectTimeout,
    IdleTimeout,
    MinRate,
    PerIpMaxConns,
    Ipv6Prefix,
    PerIpConnRate,
    PerIpBandwidth,
    Bandwidth,
    TableSize,
    TableFull,
}

/// A directive: its name, scope, number of values and, for numbers, its
/// range.
struct Directive {
    name: &'static str,
    field: Field,
    in_listener: bool,
    min_args: usize,
    max_args: usize,
    min: u64,
    max: u64,
}

const fn dir(
    name: &'static str,
    field: Field,
    in_listener: bool,
    max_args: usize,
    min: u64,
    max: u64,
) -> Directive {
    Directive {
        name,
        field,
        in_listener,
        min_args: 1,
        max_args,
        min,
        max,
    }
}

/// In config.c's order, which decides nothing but is kept for diffing.
const DIRECTIVES: [Directive; 18] = [
    dir("workers", Field::Workers, false, 1, 1, 64),
    dir("admin_socket", Field::AdminSocket, false, 1, 0, 0),
    dir(
        "drain_timeout",
        Field::DrainTimeout,
        false,
        1,
        0,
        3600 * SEC,
    ),
    dir("log_level", Field::LogLevel, false, 1, 0, 0),
    dir("listen", Field::Listen, true, 1, 0, 0),
    dir("upstream", Field::Upstream, true, 1, 0, 0),
    dir("max_conns", Field::MaxConns, true, 1, 1, 1_000_000),
    dir("buffer", Field::Buffer, true, 1, 1024, 1 << 20),
    dir(
        "connect_timeout",
        Field::ConnectTimeout,
        true,
        1,
        MS,
        600 * SEC,
    ),
    dir(
        "idle_timeout",
        Field::IdleTimeout,
        true,
        1,
        SEC,
        86_400 * SEC,
    ),
    dir("min_rate", Field::MinRate, true, 3, 0, 0),
    dir(
        "per_ip_max_conns",
        Field::PerIpMaxConns,
        true,
        1,
        0,
        1_000_000,
    ),
    dir("ipv6_prefix", Field::Ipv6Prefix, true, 1, 32, 128),
    dir("per_ip_conn_rate", Field::PerIpConnRate, true, 3, 0, 0),
    dir("per_ip_bandwidth", Field::PerIpBandwidth, true, 3, 0, 0),
    dir("bandwidth", Field::Bandwidth, true, 3, 0, 0),
    dir("table_size", Field::TableSize, true, 1, 16, 1 << 24),
    dir("table_full", Field::TableFull, true, 1, 0, 0),
];

fn directive(name: &[u8]) -> Option<(usize, &'static Directive)> {
    DIRECTIVES
        .iter()
        .enumerate()
        .find(|(_, d)| d.name.as_bytes() == name)
}

/// A listener being read: its addresses are unset until given.
struct Pending {
    conf: ListenerConfig,
    has_listen: bool,
    has_upstream: bool,
}

impl Pending {
    fn new(name: String) -> Self {
        let unset = SocketAddr::from(([0, 0, 0, 0], 0));
        Self {
            conf: ListenerConfig::new(name, unset, unset),
            has_listen: false,
            has_upstream: false,
        }
    }
}

/// Where a directive's value goes.
enum Target<'a> {
    Top(&'a mut LimiterConfig),
    Listener(&'a mut Pending),
}

impl Directive {
    /// `name: expected WHAT, got "TOKEN"`.
    fn bad(&self, what: &str, tok: &[u8]) -> String {
        format!("{}: expected {what}, got \"{}\"", self.name, shown(tok))
    }

    fn out_of_range(&self, unit: &str) -> String {
        match unit {
            "ms" => format!(
                "{}: must be between {}ms and {}ms",
                self.name,
                self.min / MS,
                self.max / MS
            ),
            _ => format!(
                "{}: must be between {} and {}{unit}",
                self.name, self.min, self.max
            ),
        }
    }

    fn whole(&self, tok: &[u8]) -> Result<u32, String> {
        let v = parse_u64(tok).ok_or_else(|| self.bad("a whole number", tok))?;
        if v < self.min || v > self.max {
            return Err(self.out_of_range(""));
        }
        // Every range is within u32.
        u32::try_from(v).map_err(|_| self.out_of_range(""))
    }

    fn size32(&self, tok: &[u8]) -> Result<u32, String> {
        let v = parse_size(tok).ok_or_else(|| self.bad("a size like 16k", tok))?;
        if v < self.min || v > self.max {
            return Err(self.out_of_range(" bytes"));
        }
        u32::try_from(v).map_err(|_| self.out_of_range(" bytes"))
    }

    fn duration(&self, tok: &[u8]) -> Result<Duration, String> {
        let v = parse_duration(tok).ok_or_else(|| self.bad("a duration like 5s or 250ms", tok))?;
        if v < self.min || v > self.max {
            return Err(self.out_of_range("ms"));
        }
        Ok(Duration::from_nanos(v))
    }

    fn addr(&self, tok: &[u8]) -> Result<SocketAddr, String> {
        parse_addr(tok).ok_or_else(|| self.bad("an address like 127.0.0.1:8080 or [::1]:8080", tok))
    }

    /// `RATE [burst SIZE]`.
    fn rate(&self, args: &[&[u8]]) -> Result<Rate, String> {
        let mut rate = parse_rate(args[0])
            .ok_or_else(|| self.bad("a rate like 10/s, 600/m or 1m/s", args[0]))?;
        match args.len() {
            1 => {}
            3 => {
                if args[1] != b"burst" {
                    return Err(self.bad("\"burst\"", args[1]));
                }
                rate.burst = parse_size(args[2])
                    .filter(|&b| b != 0)
                    .ok_or_else(|| self.bad("a burst like 20 or 64k", args[2]))?;
            }
            _ => return Err(format!("{}: expected RATE or RATE burst N", self.name)),
        }
        if rate.ticks().is_err() {
            return Err(format!(
                "{}: rate and burst are too large together",
                self.name
            ));
        }
        Ok(rate)
    }

    /// `BYTES per DURATION`, or `off`.
    fn min_rate(&self, args: &[&[u8]]) -> Result<Option<MinRate>, String> {
        if args == [b"off"] {
            return Ok(None);
        }
        if args.len() != 3 || args[1] != b"per" {
            return Err(format!(
                "{}: expected BYTES per DURATION, or off",
                self.name
            ));
        }
        let bytes = parse_size(args[0])
            .filter(|&b| b != 0)
            .ok_or_else(|| self.bad("a byte count of at least 1", args[0]))?;
        let window = parse_duration(args[2])
            .filter(|w| (SEC..=600 * SEC).contains(w))
            .ok_or_else(|| self.bad("a window between 1s and 10m", args[2]))?;
        Ok(Some(MinRate {
            bytes,
            window: Duration::from_nanos(window),
        }))
    }

    /// Checks the number of values, parses them and stores the result. The
    /// message starts with the directive's name.
    fn apply(&self, target: Target<'_>, args: &[&[u8]]) -> Result<(), String> {
        if args.len() < self.min_args || args.len() > self.max_args {
            return Err(format!("{}: wrong number of values", self.name));
        }
        let tok = args[0];
        match target {
            Target::Top(c) => match self.field {
                Field::Workers => c.workers = self.whole(tok)?,
                Field::AdminSocket => {
                    if tok.first() != Some(&b'/') || tok.len() >= ADMIN_PATH_MAX || tok.contains(&0)
                    {
                        return Err(self.bad("an absolute path shorter than 108 bytes", tok));
                    }
                    c.admin_socket =
                        Some(PathBuf::from(std::ffi::OsString::from_vec(tok.to_vec())));
                }
                Field::DrainTimeout => c.drain_timeout = self.duration(tok)?,
                Field::LogLevel => {
                    c.log_level = LogLevel::ALL
                        .iter()
                        .find(|(_, name)| name.as_bytes() == tok)
                        .map(|&(level, _)| level)
                        .ok_or_else(|| self.bad("debug, info, warn or error", tok))?;
                }
                _ => unreachable!("{} is a listener directive", self.name),
            },
            Target::Listener(p) => {
                let l = &mut p.conf;
                match self.field {
                    Field::Listen => {
                        l.listen = self.addr(tok)?;
                        p.has_listen = true;
                    }
                    Field::Upstream => {
                        l.upstream = self.addr(tok)?;
                        p.has_upstream = true;
                    }
                    Field::MaxConns => l.max_conns = self.whole(tok)?,
                    Field::Buffer => l.buffer_size = self.size32(tok)?,
                    Field::ConnectTimeout => l.connect_timeout = self.duration(tok)?,
                    Field::IdleTimeout => l.idle_timeout = self.duration(tok)?,
                    Field::MinRate => l.min_rate = self.min_rate(args)?,
                    Field::PerIpMaxConns => l.per_ip_max_conns = self.whole(tok)?,
                    Field::Ipv6Prefix => l.ipv6_prefix = self.whole(tok)?,
                    Field::PerIpConnRate => l.per_ip_conn_rate = Some(self.rate(args)?),
                    Field::PerIpBandwidth => l.per_ip_bandwidth = Some(self.rate(args)?),
                    Field::Bandwidth => l.bandwidth = Some(self.rate(args)?),
                    Field::TableSize => l.table_size = self.whole(tok)?,
                    Field::TableFull => {
                        l.table_full = match tok {
                            b"deny" => FullPolicy::Deny,
                            b"evict" => FullPolicy::Evict,
                            _ => return Err(self.bad("deny or evict", tok)),
                        }
                    }
                    _ => unreachable!("{} is a top-level directive", self.name),
                }
            }
        }
        Ok(())
    }
}

/// Checks a listener being closed against the ones before it.
fn close_listener(done: &[ListenerConfig], p: Pending) -> Result<ListenerConfig, String> {
    let l = p.conf;
    if !p.has_listen {
        return Err(format!("listener {}: missing listen", l.name));
    }
    if !p.has_upstream {
        return Err(format!("listener {}: missing upstream", l.name));
    }
    if let Some(other) = done.iter().find(|o| o.listen == l.listen) {
        return Err(format!(
            "listener {}: listen address already used by {}",
            l.name, other.name
        ));
    }
    Ok(l)
}

/// Checks a listener being opened against the ones before it.
fn open_listener(done: &[ListenerConfig], name: &[u8]) -> Result<Pending, String> {
    if !valid_name(name) {
        return Err(name_error(name));
    }
    // valid_name leaves only ASCII.
    let name = String::from_utf8_lossy(name).into_owned();
    if done.iter().any(|l| l.name == name) {
        return Err(format!("listener {name} defined twice"));
    }
    if done.len() == MAX_LISTENERS {
        return Err(format!("at most {MAX_LISTENERS} listeners"));
    }
    Ok(Pending::new(name))
}

// ---------------------------------------------------------------------------
// The pacerd.conf reader.

/// Splits a line into words separated by spaces, tabs and carriage returns,
/// dropping a `#` comment, which may start mid-word. `None` if there are
/// more than eight words.
fn tokenize(line: &[u8]) -> Option<Vec<&[u8]>> {
    let is_space = |c: u8| c == b' ' || c == b'\t' || c == b'\r';
    let mut tokens = Vec::new();
    let mut i = 0;
    loop {
        while i < line.len() && is_space(line[i]) {
            i += 1;
        }
        if i == line.len() || line[i] == b'#' {
            return Some(tokens);
        }
        if tokens.len() == MAX_TOKENS {
            return None;
        }
        let start = i;
        while i < line.len() && !is_space(line[i]) && line[i] != b'#' {
            i += 1;
        }
        tokens.push(&line[start..i]);
        if i < line.len() && line[i] == b'#' {
            return Some(tokens);
        }
    }
}

struct Parser {
    cfg: LimiterConfig,
    map: SourceMap,
    line_no: usize,
    cur: Option<Pending>,
    /// A bit per directive already set in the current scope.
    seen: u32,
    /// The top-level bits while inside a listener block.
    top_seen: u32,
}

impl Parser {
    fn line(&mut self, line: &[u8]) -> Result<(), String> {
        let tok = tokenize(line).ok_or("too many words on one line")?;
        let Some(&first) = tok.first() else {
            return Ok(());
        };
        if first == b"listener" {
            if self.cur.is_some() {
                return Err("listener blocks cannot be nested".into());
            }
            if tok.len() != 3 || tok[2] != b"{" {
                return Err("expected: listener NAME {".into());
            }
            self.cur = Some(open_listener(&self.cfg.listeners, tok[1])?);
            self.map.listeners.push(ListenerLines::new(self.line_no));
            self.top_seen = self.seen;
            self.seen = 0;
            return Ok(());
        }
        if first == b"}" {
            let Some(p) = self.cur.take().filter(|_| tok.len() == 1) else {
                return Err("unexpected }".into());
            };
            let l = close_listener(&self.cfg.listeners, p)?;
            self.cfg.listeners.push(l);
            self.seen = self.top_seen;
            return Ok(());
        }
        let Some((i, d)) = directive(first) else {
            return Err(format!("unknown directive \"{}\"", shown(first)));
        };
        if d.in_listener != self.cur.is_some() {
            let place = if d.in_listener {
                "inside a listener block"
            } else {
                "outside listener blocks"
            };
            return Err(format!("{} belongs {place}", d.name));
        }
        if self.seen & (1 << i) != 0 {
            return Err(format!("{} set twice", d.name));
        }
        self.seen |= 1 << i;
        match self.map.listeners.last_mut().filter(|_| self.cur.is_some()) {
            Some(l) => l.set(d.name, self.line_no),
            None => self.map.top.set(d.name, self.line_no),
        }
        let target = match self.cur.as_mut() {
            Some(p) => Target::Listener(p),
            None => Target::Top(&mut self.cfg),
        };
        d.apply(target, &tok[1..])
    }
}

/// Parses pacerd.conf text exactly as pacerd's `pd_config_parse` did.
pub fn parse_pacerd(buf: &[u8]) -> Result<LimiterConfig, ConfigError> {
    parse_pacerd_mapped(buf).map(|(cfg, _)| cfg)
}

/// [`parse_pacerd`], also returning the line of every directive, for
/// [`crate::lint`].
pub fn parse_pacerd_mapped(buf: &[u8]) -> Result<(LimiterConfig, SourceMap), ConfigError> {
    let fail = |line: usize, message: String| ConfigError { line, message };
    if buf.len() > MAX_FILE_BYTES {
        return Err(fail(
            0,
            format!("config larger than {MAX_FILE_BYTES} bytes"),
        ));
    }
    let mut p = Parser {
        cfg: LimiterConfig::default(),
        map: SourceMap::default(),
        line_no: 0,
        cur: None,
        seen: 0,
        top_seen: 0,
    };
    let mut line_no = 0;
    let mut rest = buf;
    while !rest.is_empty() {
        line_no += 1;
        let end = rest.iter().position(|&c| c == b'\n').unwrap_or(rest.len());
        let line = &rest[..end];
        rest = rest.get(end + 1..).unwrap_or_default();
        if line.len() > LINE_MAX {
            return Err(fail(line_no, format!("line longer than {LINE_MAX} bytes")));
        }
        if line.contains(&0) {
            return Err(fail(line_no, "unexpected NUL byte".into()));
        }
        p.line_no = line_no;
        p.line(line).map_err(|m| fail(line_no, m))?;
    }
    if let Some(open) = p.cur {
        return Err(fail(
            line_no,
            format!("listener {}: missing }}", open.conf.name),
        ));
    }
    if p.cfg.listeners.is_empty() {
        return Err(fail(line_no, "no listener defined".into()));
    }
    Ok((p.cfg, p.map))
}

/// Reads a pacerd.conf of at most [`MAX_FILE_BYTES`] and parses it. The
/// message names the file for I/O errors and gives the line otherwise.
pub fn read_pacerd(path: &Path) -> Result<LimiterConfig, String> {
    read_pacerd_mapped(path).map(|(cfg, _)| cfg)
}

/// [`read_pacerd`], also returning the line of every directive.
pub fn read_pacerd_mapped(path: &Path) -> Result<(LimiterConfig, SourceMap), String> {
    let io = |e: std::io::Error| format!("{}: {e}", path.display());
    let file = std::fs::File::open(path).map_err(io)?;
    let mut buf = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(io)?;
    parse_pacerd_mapped(&buf).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Writing values back in pacerd syntax.

fn fmt_size(v: u64) -> String {
    for (shift, unit) in [(30, "g"), (20, "m"), (10, "k")] {
        if v != 0 && v.trailing_zeros() >= shift {
            return format!("{}{unit}", v >> shift);
        }
    }
    v.to_string()
}

fn fmt_duration(field: &str, d: Duration) -> Result<String, String> {
    let ns = d.as_nanos();
    if !ns.is_multiple_of(u128::from(MS)) || ns > u128::from(MAX_DUR) {
        return Err(format!(
            "{field}: {d:?} is not a whole number of milliseconds up to 24h"
        ));
    }
    for (unit_ns, unit) in [(3600 * SEC, "h"), (60 * SEC, "m"), (SEC, "s")] {
        if ns != 0 && ns.is_multiple_of(u128::from(unit_ns)) {
            return Ok(format!("{}{unit}", ns / u128::from(unit_ns)));
        }
    }
    Ok(format!("{}ms", ns / u128::from(MS)))
}

fn fmt_rate(field: &str, r: &Rate) -> Result<String, String> {
    let unit = match r.period_ns {
        SEC => "s",
        p if p == 60 * SEC => "m",
        p if p == 3600 * SEC => "h",
        _ => {
            return Err(format!(
                "{field}: a period of {}ns is not a second, minute or hour",
                r.period_ns
            ));
        }
    };
    let mut s = format!("{}/{unit}", fmt_size(r.tokens));
    if r.burst != r.tokens {
        s.push_str(" burst ");
        s.push_str(&fmt_size(r.burst));
    }
    Ok(s)
}

/// A value as either reader takes it: an integer or pacerd's text.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    Int(u64),
    Text(String),
}

impl Value {
    /// The value's words, as pacerd would have split them.
    fn words(&self) -> Vec<Vec<u8>> {
        match self {
            Value::Int(n) => vec![n.to_string().into_bytes()],
            Value::Text(s) => s
                .split([' ', '\t', '\r', '\n'])
                .filter(|w| !w.is_empty())
                .map(|w| w.as_bytes().to_vec())
                .collect(),
        }
    }
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Int(n) => s.serialize_u64(*n),
            Value::Text(t) => s.serialize_str(t),
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Value;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string or an integer")
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
                Ok(Value::Int(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
                // Negative numbers reach the parsers, which name them.
                Ok(u64::try_from(v).map_or_else(|_| Value::Text(v.to_string()), Value::Int))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
                Ok(Value::Text(v.to_owned()))
            }
        }
        d.deserialize_any(V)
    }
}

type Entries = Vec<(&'static str, Value)>;

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workers: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    admin_socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    drain_timeout: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    log_level: Option<Value>,
    #[serde(default)]
    listeners: Vec<RawListener>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawListener {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    listen: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    upstream: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_conns: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    buffer: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connect_timeout: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idle_timeout: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min_rate: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    per_ip_max_conns: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ipv6_prefix: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    per_ip_conn_rate: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    per_ip_bandwidth: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bandwidth: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    table_size: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    table_full: Option<Value>,
}

impl RawConfig {
    /// The top-level values in directive order. The admin socket is one
    /// word whatever it contains.
    fn entries(&self) -> Entries {
        let mut out = Entries::new();
        let mut put = |name, v: &Option<Value>| {
            if let Some(v) = v {
                out.push((name, v.clone()));
            }
        };
        put("workers", &self.workers);
        put("drain_timeout", &self.drain_timeout);
        put("log_level", &self.log_level);
        out
    }
}

impl RawListener {
    fn entries(&self) -> Entries {
        let mut out = Entries::new();
        let mut put = |name, v: &Option<Value>| {
            if let Some(v) = v {
                out.push((name, v.clone()));
            }
        };
        put("listen", &self.listen);
        put("upstream", &self.upstream);
        put("max_conns", &self.max_conns);
        put("buffer", &self.buffer);
        put("connect_timeout", &self.connect_timeout);
        put("idle_timeout", &self.idle_timeout);
        put("min_rate", &self.min_rate);
        put("per_ip_max_conns", &self.per_ip_max_conns);
        put("ipv6_prefix", &self.ipv6_prefix);
        put("per_ip_conn_rate", &self.per_ip_conn_rate);
        put("per_ip_bandwidth", &self.per_ip_bandwidth);
        put("bandwidth", &self.bandwidth);
        put("table_size", &self.table_size);
        put("table_full", &self.table_full);
        out
    }
}

fn apply_words(target: Target<'_>, name: &str, words: &[Vec<u8>]) -> Result<(), String> {
    let (_, d) = directive(name.as_bytes()).expect("a known directive");
    let args: Vec<&[u8]> = words.iter().map(Vec::as_slice).collect();
    d.apply(target, &args)
}

impl TryFrom<RawConfig> for LimiterConfig {
    type Error = String;

    fn try_from(raw: RawConfig) -> Result<Self, String> {
        let mut cfg = LimiterConfig::default();
        if let Some(path) = &raw.admin_socket {
            apply_words(
                Target::Top(&mut cfg),
                "admin_socket",
                &[path.as_bytes().to_vec()],
            )?;
        }
        for (name, v) in raw.entries() {
            apply_words(Target::Top(&mut cfg), name, &v.words())?;
        }
        if raw.listeners.is_empty() {
            return Err("listeners: no listener defined".into());
        }
        for (i, rl) in raw.listeners.iter().enumerate() {
            let at = |m: String| format!("listeners[{i}].{m}");
            let name = rl
                .name
                .as_deref()
                .ok_or_else(|| at("name: missing".into()))?;
            let mut p = open_listener(&cfg.listeners, name.as_bytes())
                .map_err(|m| format!("listeners[{i}]: {m}"))?;
            for (field, v) in rl.entries() {
                apply_words(Target::Listener(&mut p), field, &v.words()).map_err(at)?;
            }
            let l =
                close_listener(&cfg.listeners, p).map_err(|m| format!("listeners[{i}]: {m}"))?;
            cfg.listeners.push(l);
        }
        Ok(cfg)
    }
}

impl LimiterConfig {
    /// The config as serde values. The admin socket is left to the caller,
    /// because pacerd accepts paths that are not UTF-8.
    fn to_raw(&self) -> Result<RawConfig, String> {
        let text = |s: String| Some(Value::Text(s));
        let mut raw = RawConfig {
            workers: Some(Value::Int(self.workers.into())),
            admin_socket: None,
            drain_timeout: text(fmt_duration("drain_timeout", self.drain_timeout)?),
            log_level: text(self.log_level.as_str().to_owned()),
            listeners: Vec::new(),
        };
        for (i, l) in self.listeners.iter().enumerate() {
            let at = |m: String| format!("listeners[{i}].{m}");
            let rate = |field, r: &Option<Rate>| {
                r.as_ref()
                    .map(|r| fmt_rate(field, r).map(Value::Text))
                    .transpose()
                    .map_err(at)
            };
            let addr = |field: &str, a: SocketAddr| match a {
                SocketAddr::V6(v6) if v6.flowinfo() != 0 || v6.scope_id() != 0 => Err(at(format!(
                    "{field}: {a} has a flow label or scope id, which cannot be configured"
                ))),
                _ => Ok(Some(Value::Text(a.to_string()))),
            };
            let min_rate = match l.min_rate {
                None => "off".to_owned(),
                Some(m) => format!(
                    "{} per {}",
                    fmt_size(m.bytes),
                    fmt_duration("min_rate", m.window).map_err(at)?
                ),
            };
            raw.listeners.push(RawListener {
                name: Some(l.name.clone()),
                listen: addr("listen", l.listen)?,
                upstream: addr("upstream", l.upstream)?,
                max_conns: Some(Value::Int(l.max_conns.into())),
                buffer: text(fmt_size(l.buffer_size.into())),
                connect_timeout: text(
                    fmt_duration("connect_timeout", l.connect_timeout).map_err(at)?,
                ),
                idle_timeout: text(fmt_duration("idle_timeout", l.idle_timeout).map_err(at)?),
                min_rate: text(min_rate),
                per_ip_max_conns: Some(Value::Int(l.per_ip_max_conns.into())),
                ipv6_prefix: Some(Value::Int(l.ipv6_prefix.into())),
                per_ip_conn_rate: rate("per_ip_conn_rate", &l.per_ip_conn_rate)?,
                per_ip_bandwidth: rate("per_ip_bandwidth", &l.per_ip_bandwidth)?,
                bandwidth: rate("bandwidth", &l.bandwidth)?,
                table_size: Some(Value::Int(l.table_size.into())),
                table_full: text(
                    match l.table_full {
                        FullPolicy::Deny => "deny",
                        FullPolicy::Evict => "evict",
                    }
                    .to_owned(),
                ),
            });
        }
        Ok(raw)
    }

    /// Checks a config built in code against the rules both readers apply,
    /// by writing it out and reading it back. The message names the field,
    /// as the serde reader does.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(path) = &self.admin_socket {
            let mut scratch = LimiterConfig::default();
            apply_words(
                Target::Top(&mut scratch),
                "admin_socket",
                &[path.as_os_str().as_bytes().to_vec()],
            )?;
        }
        LimiterConfig::try_from(self.to_raw()?).map(drop)
    }

    /// Writes the config as a pacerd.conf that [`parse_pacerd`] reads back
    /// into an equal config. Fails if the config is invalid or cannot be
    /// written in that syntax (an admin path with spaces, tabs, `#` or a
    /// newline in it).
    pub fn to_pacerd(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        let raw = self.to_raw()?;
        let mut out = Vec::new();
        let line = |out: &mut Vec<u8>, indent: &str, name: &str, words: &[u8]| {
            out.extend_from_slice(indent.as_bytes());
            out.extend_from_slice(name.as_bytes());
            out.push(b' ');
            out.extend_from_slice(words);
            out.push(b'\n');
        };
        let text = |v: &Value| match v {
            Value::Int(n) => n.to_string(),
            Value::Text(t) => t.clone(),
        };
        for (name, v) in raw.entries() {
            line(&mut out, "", name, text(&v).as_bytes());
        }
        if let Some(path) = &self.admin_socket {
            let bytes = path.as_os_str().as_bytes();
            if bytes
                .iter()
                .any(|&c| matches!(c, b' ' | b'\t' | b'\r' | b'\n' | b'#'))
            {
                return Err(
                    "admin_socket: a path with spaces, tabs, newlines or # cannot be written in pacerd syntax"
                        .into(),
                );
            }
            line(&mut out, "", "admin_socket", bytes);
        }
        for rl in &raw.listeners {
            let name = rl.name.as_deref().unwrap_or_default();
            out.extend_from_slice(format!("\nlistener {name} {{\n").as_bytes());
            for (field, v) in rl.entries() {
                line(&mut out, "    ", field, text(&v).as_bytes());
            }
            out.extend_from_slice(b"}\n");
        }
        Ok(out)
    }
}

impl Serialize for LimiterConfig {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(ser::Error::custom)?;
        let mut raw = self.to_raw().map_err(ser::Error::custom)?;
        if let Some(path) = &self.admin_socket {
            let path = path
                .to_str()
                .ok_or_else(|| ser::Error::custom("admin_socket: the path is not UTF-8"))?;
            raw.admin_socket = Some(path.to_owned());
        }
        raw.serialize(s)
    }
}

impl<'de> Deserialize<'de> for LimiterConfig {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        LimiterConfig::try_from(RawConfig::deserialize(d)?).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests;
