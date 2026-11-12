//! Lints for limiter configs that parse but do not do what they say.
//!
//! [`lint`] looks at a valid [`LimiterConfig`] and reports each setting
//! that cannot take effect, takes effect in a way its text does not
//! suggest, or turns a limit against the clients it should serve. Every
//! finding names a [`Class`], the listener and the line of the directive
//! at fault, taken from a [`SourceMap`]; a directive left at its default
//! is reported on the line that opens its listener. Findings are warnings:
//! pacerd would run the config, and the lints say how it would behave.
//!
//! Each class rests on what pacerd's data path does with the value:
//!
//! | class | finding |
//! |---|---|
//! | `burst-below-read` | a byte limit's burst is below the 4 KiB read quantum, so every shaped read is cut to the burst |
//! | `listen-overlap` | a listener's address and a wildcard listener's address share a port, so the specific one takes that address's connections from the wildcard one |
//! | `table-below-max-conns` | per-client admission tracks fewer clients than `max_conns`, so distinct clients are refused before the listener is full |
//! | `per-ip-cap-unreachable` | `per_ip_max_conns` is at least `max_conns`, so it never refuses anyone |
//! | `per-ip-bandwidth-unreachable` | `per_ip_bandwidth` is at least `bandwidth` in rate and burst, so the listener's limit always binds first |
//! | `rate-rounding` | the rate's interval rounds up by more than 1%, so it is enforced slower than written |
//! | `unused-table` | `table_size` or `table_full` is set but no per-client limit uses the table |
//! | `ipv6-prefix-unused` | `ipv6_prefix` is set on a listener that never groups IPv6 clients |
//! | `wide-ipv6-prefix` | per-client limits group IPv6 clients by a prefix shorter than /48, so whole sites share one limit |
//! | `upstream-loop` | the upstream is a listener's own address, so connections loop back into the limiter |

use std::fmt;
use std::net::SocketAddr;

use crate::config::{LimiterConfig, ListenerConfig, ListenerLines, SourceMap};
use crate::gcra::{Rate, TICK_SHIFT};
use crate::rate::bucket_slowdown;

/// pacerd's smallest shaped read: a byte limiter is asked for at least
/// this many bytes, or the whole buffer if it is smaller (`READ_QUANTUM`
/// in proxy.c). A burst below it caps the read.
pub const READ_QUANTUM: u64 = 4096;

/// The shortest IPv6 prefix that still groups one site: /48 is the usual
/// end-site assignment, so a shorter prefix puts several sites, or a whole
/// provider, behind one per-client limit.
pub const SITE_PREFIX: u32 = 48;

/// The largest slowdown, as `1 / ROUNDING_DEN`, that `rate-rounding`
/// lets pass: 1%. pacerd's byte rates are commonly a little over 0.1%
/// slow (`100m/s` by 0.106%), which is noise for a bandwidth cap; past 1%
/// the rate written is no longer the rate enforced, and above 64 billion a
/// second the interval rounds up from below one tick.
const ROUNDING_DEN: u128 = 100;

/// What a lint found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Class {
    BurstBelowRead,
    ListenOverlap,
    TableBelowMaxConns,
    PerIpCapUnreachable,
    PerIpBandwidthUnreachable,
    RateRounding,
    UnusedTable,
    Ipv6PrefixUnused,
    WideIpv6Prefix,
    UpstreamLoop,
}

impl Class {
    /// Every class, in the order of the table in the module docs.
    pub const ALL: [Class; 10] = [
        Class::BurstBelowRead,
        Class::ListenOverlap,
        Class::TableBelowMaxConns,
        Class::PerIpCapUnreachable,
        Class::PerIpBandwidthUnreachable,
        Class::RateRounding,
        Class::UnusedTable,
        Class::Ipv6PrefixUnused,
        Class::WideIpv6Prefix,
        Class::UpstreamLoop,
    ];

    /// The class's name, as `edgefront check` prints it and `--allow`
    /// takes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Class::BurstBelowRead => "burst-below-read",
            Class::ListenOverlap => "listen-overlap",
            Class::TableBelowMaxConns => "table-below-max-conns",
            Class::PerIpCapUnreachable => "per-ip-cap-unreachable",
            Class::PerIpBandwidthUnreachable => "per-ip-bandwidth-unreachable",
            Class::RateRounding => "rate-rounding",
            Class::UnusedTable => "unused-table",
            Class::Ipv6PrefixUnused => "ipv6-prefix-unused",
            Class::WideIpv6Prefix => "wide-ipv6-prefix",
            Class::UpstreamLoop => "upstream-loop",
        }
    }

    /// The class named `name`.
    pub fn from_name(name: &str) -> Option<Class> {
        Class::ALL.into_iter().find(|c| c.as_str() == name)
    }
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One finding: the line of the directive at fault (0 when the config has
/// no source map), the listener and what is wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lint {
    pub line: usize,
    pub class: Class,
    pub listener: String,
    pub message: String,
}

impl fmt::Display for Lint {
    /// `warning[CLASS]: listener NAME: MESSAGE`; the caller puts the file
    /// and line in front.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "warning[{}]: listener {}: {}",
            self.class, self.listener, self.message
        )
    }
}

/// Lints `cfg`, naming lines from `map`. `map` may be empty, as for a
/// config built in code: every line is then 0, and the lints that depend
/// on whether a directive was written out (`unused-table` and
/// `ipv6-prefix-unused`) are skipped. Findings come sorted by line, then
/// class.
pub fn lint(cfg: &LimiterConfig, map: &SourceMap) -> Vec<Lint> {
    let mut out = Vec::new();
    let unmapped = ListenerLines::new(0);
    for (i, l) in cfg.listeners.iter().enumerate() {
        let (lines, mapped) = match map.listeners.get(i) {
            Some(lines) => (lines, true),
            None => (&unmapped, false),
        };
        let mut push = |directive: &str, class: Class, message: String| {
            out.push(Lint {
                line: lines.line_of(directive),
                class,
                listener: l.name.clone(),
                message,
            });
        };
        listener_lints(l, mapped.then_some(lines), &mut push);
        for (j, other) in cfg.listeners.iter().enumerate() {
            if let Some(m) = overlap(l, other).filter(|_| j < i) {
                push("listen", Class::ListenOverlap, m);
            }
            if loops_into(other.listen, l.upstream) {
                let whose = if i == j {
                    "its own listen address".to_owned()
                } else {
                    format!("served by listener {} ({})", other.name, other.listen)
                };
                push(
                    "upstream",
                    Class::UpstreamLoop,
                    format!(
                        "upstream {} is {whose}, so every connection loops back into the limiter",
                        l.upstream
                    ),
                );
            }
        }
    }
    out.sort_by_key(|l| (l.line, l.class));
    out
}

/// Whether the listener admits connections through the per-client table
/// (proxy.c's `admit`).
fn per_ip_admission(l: &ListenerConfig) -> bool {
    l.per_ip_max_conns != 0 || l.per_ip_conn_rate.is_some()
}

/// Whether any per-client limit keys the client table.
fn uses_table(l: &ListenerConfig) -> bool {
    per_ip_admission(l) || l.per_ip_bandwidth.is_some()
}

/// `tokens/period` and `burst` as written, such as `1m/s burst 2k`.
fn shown(r: &Rate) -> String {
    let unit = match r.period_ns {
        1_000_000_000 => "s".to_owned(),
        60_000_000_000 => "m".to_owned(),
        3_600_000_000_000 => "h".to_owned(),
        p => format!("{p}ns"),
    };
    let mut s = format!("{}/{unit}", r.tokens);
    if r.burst != r.tokens {
        s.push_str(&format!(" burst {}", r.burst));
    }
    s
}

/// Tokens per second, for messages.
fn per_second(r: &Rate) -> f64 {
    r.tokens as f64 * 1e9 / r.period_ns as f64
}

fn listener_lints(
    l: &ListenerConfig,
    lines: Option<&ListenerLines>,
    push: &mut impl FnMut(&str, Class, String),
) {
    let rates = [
        ("per_ip_conn_rate", &l.per_ip_conn_rate),
        ("per_ip_bandwidth", &l.per_ip_bandwidth),
        ("bandwidth", &l.bandwidth),
    ];

    // A byte limiter is asked for min(free buffer, 4 KiB) at least, and
    // libpacer clamps that minimum to the burst: the read is cut, not
    // refused, and the connection wakes once per burst of bytes.
    let quantum = READ_QUANTUM.min(u64::from(l.buffer_size));
    for (name, rate) in &rates[1..] {
        if let Some(r) = rate.as_ref().filter(|r| r.burst < quantum) {
            push(
                name,
                Class::BurstBelowRead,
                format!(
                    "{name} {}: a burst of {} bytes is below the {quantum}-byte read, so each shaped read moves at most {} bytes and a busy connection wakes about {:.0} times a second",
                    shown(r),
                    r.burst,
                    r.burst,
                    per_second(r) / r.burst as f64
                ),
            );
        }
    }

    if per_ip_admission(l) && l.table_size < l.max_conns {
        push(
            "table_size",
            Class::TableBelowMaxConns,
            format!(
                "table_size {} is below max_conns {}: a client with open connections is never evicted, so once {} different clients are connected the next new client is refused as table_full, whatever table_full says",
                l.table_size, l.max_conns, l.table_size
            ),
        );
    }

    if l.per_ip_max_conns != 0 && l.per_ip_max_conns >= l.max_conns {
        push(
            "per_ip_max_conns",
            Class::PerIpCapUnreachable,
            format!(
                "per_ip_max_conns {} is not below max_conns {}, so one client reaches the listener's limit first and the per-client cap never refuses anyone",
                l.per_ip_max_conns, l.max_conns
            ),
        );
    }

    if let (Some(ip), Some(all)) = (&l.per_ip_bandwidth, &l.bandwidth) {
        // ip.tokens / ip.period >= all.tokens / all.period, exactly.
        let faster = u128::from(ip.tokens) * u128::from(all.period_ns)
            >= u128::from(all.tokens) * u128::from(ip.period_ns);
        if faster && ip.burst >= all.burst {
            push(
                "per_ip_bandwidth",
                Class::PerIpBandwidthUnreachable,
                format!(
                    "per_ip_bandwidth {} is at least bandwidth {} in both rate and burst, so the listener's limit always binds first and the per-client one never does",
                    shown(ip),
                    shown(all)
                ),
            );
        }
    }

    for (name, rate) in rates {
        let Some(r) = rate else { continue };
        let Some((slow, of)) = bucket_slowdown(r) else {
            continue;
        };
        if slow * ROUNDING_DEN > of {
            let exact = (u128::from(r.period_ns) << TICK_SHIFT) as f64 / r.tokens as f64;
            let enforced = of as f64 / (of + slow) as f64;
            let how = if exact < 1.0 {
                format!(
                    "its interval of {exact:.3} ticks of 1/64 ns rounds up to 1, the shortest the limiter keeps"
                )
            } else {
                format!(
                    "its interval of {exact:.3} ticks of 1/64 ns rounds up to {}",
                    exact.ceil()
                )
            };
            push(
                name,
                Class::RateRounding,
                format!(
                    "{name} {}: {how}, so it is enforced at {:.4}% of the written rate ({:.0}/s instead of {:.0}/s)",
                    shown(r),
                    enforced * 100.0,
                    per_second(r) * enforced,
                    per_second(r)
                ),
            );
        }
    }

    let v6 = l.listen.is_ipv6();
    if v6 && uses_table(l) && l.ipv6_prefix < SITE_PREFIX {
        push(
            "ipv6_prefix",
            Class::WideIpv6Prefix,
            format!(
                "ipv6_prefix {} groups every client in a /{} as one, so whole sites or providers share each per-client limit; /{SITE_PREFIX} is one site and /64 one network",
                l.ipv6_prefix, l.ipv6_prefix
            ),
        );
    }

    // The two lints below are about directives written out for nothing,
    // so they need to know what was written.
    let Some(lines) = lines else { return };
    if !uses_table(l) {
        for name in ["table_size", "table_full"] {
            if lines.directives.get(name).is_some() {
                push(
                    name,
                    Class::UnusedTable,
                    format!(
                        "{name} has no effect: no per_ip_max_conns, per_ip_conn_rate or per_ip_bandwidth uses the client table"
                    ),
                );
            }
        }
    }
    if lines.directives.get("ipv6_prefix").is_some() {
        let why = if !v6 {
            Some(format!("{} accepts only IPv4 clients", l.listen))
        } else if !uses_table(l) {
            Some("no per-client limit groups clients".to_owned())
        } else {
            None
        };
        if let Some(why) = why {
            push(
                "ipv6_prefix",
                Class::Ipv6PrefixUnused,
                format!("ipv6_prefix has no effect: {why}"),
            );
        }
    }
}

/// Whether connecting to `upstream` surely reaches a socket listening on
/// `listen`: the same address, or a loopback or unspecified one (which
/// Linux connects to loopback) under a wildcard listener on its port.
/// IPv6 listeners are V6ONLY in pacerd, so `[::]` covers IPv6 only. Any
/// other address under a wildcard may be this host or another, so it is
/// not reported.
fn loops_into(listen: SocketAddr, upstream: SocketAddr) -> bool {
    listen.port() == upstream.port()
        && listen.is_ipv6() == upstream.is_ipv6()
        && (listen.ip() == upstream.ip()
            || (listen.ip().is_unspecified()
                && (upstream.ip().is_loopback() || upstream.ip().is_unspecified())))
}

/// The finding for `later` when it and `earlier` share a port and family
/// and one of them is a wildcard. pacerd binds each with SO_REUSEPORT, so
/// both bind and the kernel hands each connection to the most specific
/// match.
fn overlap(later: &ListenerConfig, earlier: &ListenerConfig) -> Option<String> {
    let (a, b) = (later.listen, earlier.listen);
    if a == b || a.port() != b.port() || a.is_ipv6() != b.is_ipv6() {
        return None;
    }
    let (specific, wildcard) = match (a.ip().is_unspecified(), b.ip().is_unspecified()) {
        (false, true) => (later, earlier),
        (true, false) => (earlier, later),
        _ => return None,
    };
    Some(format!(
        "{a} overlaps listener {}'s {b}: connections to {} go to listener {}, so listener {}'s limits never apply to them",
        earlier.name, specific.listen, specific.name, wildcard.name
    ))
}

#[cfg(test)]
mod tests;
