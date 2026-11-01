//! The pieces of Go's standard library that steer's config validation leans
//! on, ported so that the same text is accepted or rejected the same way:
//! `time.ParseDuration` and `Duration.String`, `net.SplitHostPort`,
//! `netip.ParseAddr` and `ParsePrefix`, `strings.ToLower`,
//! `textproto.CanonicalMIMEHeaderKey` and `path/filepath`'s `Clean`, `Dir`
//! and `Join` for slash-separated paths.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const NANOS_PER_SECOND: u64 = 1_000_000_000;
const MAX_MAGNITUDE: u64 = 1 << 63;

/// Go's `time.ParseDuration`, giving signed nanoseconds.
///
/// A duration is an optional sign and a sequence of decimal numbers, each
/// with an optional fraction and a unit (`ns`, `us`, `µs`, `μs`, `ms`, `s`,
/// `m`, `h`), such as `"300ms"`, `"-1.5h"` or `"2h45m"`; a lone `"0"` is
/// also accepted. Values beyond the range of an `i64` are rejected, except
/// that the negative range reaches `i64::MIN` as in Go.
pub(crate) fn parse_duration(text: &str) -> Option<i64> {
    let mut s = text.as_bytes();
    let mut neg = false;
    if let Some(&c) = s.first()
        && (c == b'-' || c == b'+')
    {
        neg = c == b'-';
        s = &s[1..];
    }
    if s == b"0" {
        return Some(0);
    }
    if s.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    while !s.is_empty() {
        if !(s[0] == b'.' || s[0].is_ascii_digit()) {
            return None;
        }
        let before = s.len();
        let (whole, rest) = leading_int(s)?;
        s = rest;
        let pre = before != s.len();
        let (mut frac, mut scale, mut post) = (0u64, 1f64, false);
        if s.first() == Some(&b'.') {
            s = &s[1..];
            let before = s.len();
            let (f, sc, rest) = leading_fraction(s);
            (frac, scale, s) = (f, sc, rest);
            post = before != s.len();
        }
        if !pre && !post {
            return None;
        }
        let unit_len = s
            .iter()
            .position(|&c| c == b'.' || c.is_ascii_digit())
            .unwrap_or(s.len());
        if unit_len == 0 {
            return None;
        }
        let unit = match &s[..unit_len] {
            b"ns" => 1,
            // "us", "µs" (micro sign) and "μs" (Greek mu).
            b"us" | b"\xc2\xb5s" | b"\xce\xbcs" => 1_000,
            b"ms" => 1_000_000,
            b"s" => NANOS_PER_SECOND,
            b"m" => 60 * NANOS_PER_SECOND,
            b"h" => 3600 * NANOS_PER_SECOND,
            _ => return None,
        };
        s = &s[unit_len..];
        if whole > MAX_MAGNITUDE / unit {
            return None;
        }
        let mut v = whole * unit;
        if frac > 0 {
            // Go computes the fraction in float64 and truncates.
            v += (frac as f64 * (unit as f64 / scale)) as u64;
            if v > MAX_MAGNITUDE {
                return None;
            }
        }
        total += v;
        if total > MAX_MAGNITUDE {
            return None;
        }
    }
    if neg {
        // 1 << 63 negates to i64::MIN.
        return Some((total as i64).wrapping_neg());
    }
    i64::try_from(total).ok()
}

/// Consumes leading decimal digits, failing on overflow past 1 << 63.
fn leading_int(s: &[u8]) -> Option<(u64, &[u8])> {
    let mut x: u64 = 0;
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        if x > MAX_MAGNITUDE / 10 {
            return None;
        }
        x = x * 10 + u64::from(s[i] - b'0');
        if x > MAX_MAGNITUDE {
            return None;
        }
        i += 1;
    }
    Some((x, &s[i..]))
}

/// Consumes the digits of a fraction, ignoring those beyond what fits.
fn leading_fraction(s: &[u8]) -> (u64, f64, &[u8]) {
    let (mut x, mut scale, mut overflow) = (0u64, 1f64, false);
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        if !overflow {
            if x > (MAX_MAGNITUDE - 1) / 10 {
                overflow = true;
            } else {
                let y = x * 10 + u64::from(s[i] - b'0');
                if y > MAX_MAGNITUDE {
                    overflow = true;
                } else {
                    x = y;
                    scale *= 10.0;
                }
            }
        }
        i += 1;
    }
    (x, scale, &s[i..])
}

/// Go's `time.Duration.String`: `"0s"`, `"1.5ms"`, `"2m30s"`, `"1h0m0s"`.
pub(crate) fn format_duration(nanos: i64) -> String {
    let mut u = nanos.unsigned_abs();
    let mut out = String::new();
    if u < NANOS_PER_SECOND {
        let (prec, unit) = match u {
            0 => return "0s".to_owned(),
            1..1_000 => (0, "ns"),
            1_000..1_000_000 => (3, "µs"),
            _ => (6, "ms"),
        };
        let frac = fraction(&mut u, prec);
        out.push_str(&u.to_string());
        out.push_str(&frac);
        out.push_str(unit);
    } else {
        let frac = fraction(&mut u, 9);
        let secs = u % 60;
        u /= 60;
        if u > 0 {
            let mins = u % 60;
            u /= 60;
            if u > 0 {
                out.push_str(&format!("{u}h"));
            }
            out.push_str(&format!("{mins}m"));
        }
        out.push_str(&format!("{secs}{frac}s"));
    }
    if nanos < 0 {
        out.insert(0, '-');
    }
    out
}

/// Splits off the low `prec` decimal digits of `v` as `".ddd"` without
/// trailing zeros (empty when they are all zero), leaving the rest in `v`.
fn fraction(v: &mut u64, prec: u32) -> String {
    let digits = *v % 10u64.pow(prec);
    *v /= 10u64.pow(prec);
    if digits == 0 {
        return String::new();
    }
    let s = format!(".{digits:0width$}", width = prec as usize);
    s.trim_end_matches('0').to_owned()
}

/// Go's `net.SplitHostPort`. The port may be empty or any text without a
/// colon; a bracketed host may hold colons.
pub(crate) fn split_host_port(hp: &str) -> Result<(&str, &str), &'static str> {
    let b = hp.as_bytes();
    let colon = b
        .iter()
        .rposition(|&c| c == b':')
        .ok_or("missing port in address")?;
    let (host, open_from, close_from) = if b[0] == b'[' {
        let end = b
            .iter()
            .position(|&c| c == b']')
            .ok_or("missing ']' in address")?;
        if end + 1 == b.len() {
            return Err("missing port in address");
        }
        if end + 1 != colon {
            return Err(if b[end + 1] == b':' {
                "too many colons in address"
            } else {
                "missing port in address"
            });
        }
        (&hp[1..end], 1, end + 1)
    } else {
        let host = &hp[..colon];
        if host.contains(':') {
            return Err("too many colons in address");
        }
        (host, 0, 0)
    };
    if b[open_from..].contains(&b'[') {
        return Err("unexpected '[' in address");
    }
    if b[close_from..].contains(&b']') {
        return Err("unexpected ']' in address");
    }
    Ok((host, &hp[colon + 1..]))
}

/// An address as Go's `netip.ParseAddr` returns it: IPv4, or IPv6 with an
/// optional zone. An IPv4-mapped IPv6 address stays IPv6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Addr {
    pub(crate) ip: IpAddr,
    pub(crate) zone: Option<String>,
}

/// Go's `netip.ParseAddr`: dotted-quad IPv4 without leading zeros, or IPv6
/// with an optional non-empty `%zone`.
pub(crate) fn parse_addr(s: &str) -> Option<Addr> {
    match s.bytes().find(|c| matches!(c, b'.' | b':' | b'%'))? {
        b'.' => {
            let ip = s.parse::<Ipv4Addr>().ok()?;
            Some(Addr {
                ip: IpAddr::V4(ip),
                zone: None,
            })
        }
        b':' => {
            let (addr, zone) = match s.split_once('%') {
                Some((_, "")) => return None,
                Some((a, z)) => (a, Some(z.to_owned())),
                None => (s, None),
            };
            let ip = addr.parse::<Ipv6Addr>().ok()?;
            Some(Addr {
                ip: IpAddr::V6(ip),
                zone,
            })
        }
        _ => None,
    }
}

/// Go's `netip.ParsePrefix`: `addr/bits` with no zone and canonical
/// decimal bits within the address length. The address keeps its host
/// bits; see [`mask`].
pub(crate) fn parse_prefix(s: &str) -> Option<(IpAddr, u8)> {
    let (addr, bits) = s.rsplit_once('/')?;
    let addr = parse_addr(addr)?;
    if addr.zone.is_some() {
        return None;
    }
    if bits.is_empty() || !bits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if bits.len() > 1 && bits.starts_with('0') {
        return None;
    }
    let max = if addr.ip.is_ipv4() { 32 } else { 128 };
    let bits: u32 = bits.parse().ok().filter(|&b| b <= max)?;
    Some((addr.ip, bits as u8))
}

/// Clears the bits of `ip` past the first `bits`.
pub(crate) fn mask(ip: IpAddr, bits: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let m = u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0);
            IpAddr::V4(Ipv4Addr::from(u32::from(v4) & m))
        }
        IpAddr::V6(v6) => {
            let m = u128::MAX.checked_shl(128 - u32::from(bits)).unwrap_or(0);
            IpAddr::V6(Ipv6Addr::from(u128::from(v6) & m))
        }
    }
}

/// Go's `strings.ToLower`: ASCII letters, and other characters by their
/// simple (one character) Unicode mapping, under which U+0130 (capital I
/// with dot above) becomes a plain `i`.
pub(crate) fn to_lower(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    s.chars()
        .map(|c| {
            if c == '\u{130}' {
                return 'i';
            }
            let mut lower = c.to_lowercase();
            match (lower.next(), lower.next()) {
                (Some(l), None) => l,
                _ => c,
            }
        })
        .collect()
}

/// Go's `textproto.CanonicalMIMEHeaderKey` for a valid token: upper case at
/// the start and after each hyphen, lower case elsewhere.
pub(crate) fn canonical_header_key(token: &str) -> String {
    let mut upper = true;
    token
        .bytes()
        .map(|c| {
            let out = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == b'-';
            char::from(out)
        })
        .collect()
}

/// Go's `filepath.Clean` for slash-separated paths: the shortest lexically
/// equivalent path.
pub(crate) fn clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => match parts.last() {
                Some(&last) if last != ".." => {
                    parts.pop();
                }
                _ if rooted => {}
                _ => parts.push(".."),
            },
            _ => parts.push(seg),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_owned(),
        (false, false) => joined,
    }
}

/// Go's `filepath.Dir`: everything before the last slash, cleaned.
pub(crate) fn dir(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => clean(&path[..=i]),
        None => ".".to_owned(),
    }
}

/// Go's `filepath.Join` of two elements.
pub(crate) fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => String::new(),
        (true, false) => clean(b),
        (false, true) => clean(a),
        (false, false) => clean(&format!("{a}/{b}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;
    const S: i64 = 1_000 * MS;

    #[test]
    fn parse_duration_matches_go() {
        // The table from Go's time package tests.
        let ok: &[(&str, i64)] = &[
            ("0", 0),
            ("5s", 5 * S),
            ("30s", 30 * S),
            ("1478s", 1478 * S),
            ("-5s", -5 * S),
            ("+5s", 5 * S),
            ("-0", 0),
            ("+0", 0),
            ("5.0s", 5 * S),
            ("5.6s", 5 * S + 600 * MS),
            ("5.s", 5 * S),
            (".5s", 500 * MS),
            ("1.0s", S),
            ("1.00s", S),
            ("1.004s", S + 4 * MS),
            ("1.0040s", S + 4 * MS),
            ("100.00100s", 100 * S + MS),
            ("10ns", 10),
            ("11us", 11_000),
            ("12µs", 12_000),
            ("12μs", 12_000),
            ("13ms", 13 * MS),
            ("14s", 14 * S),
            ("15m", 15 * 60 * S),
            ("16h", 16 * 3600 * S),
            ("3h30m", (3 * 3600 + 30 * 60) * S),
            ("10.5s4m", 4 * 60 * S + 10 * S + 500 * MS),
            ("-2m3.4s", -(2 * 60 * S + 3 * S + 400 * MS)),
            (
                "1h2m3s4ms5us6ns",
                3600 * S + 2 * 60 * S + 3 * S + 4 * MS + 5_000 + 6,
            ),
            (
                "39h9m14.425s",
                39 * 3600 * S + 9 * 60 * S + 14 * S + 425 * MS,
            ),
            ("52763797000ns", 52_763_797_000),
            ("0.3333333333333333333h", 20 * 60 * S),
            ("9007199254740993ns", (1 << 53) + 1),
            ("9223372036854775807ns", i64::MAX),
            ("9223372036854775.807us", i64::MAX),
            ("9223372036s854ms775us807ns", i64::MAX),
            ("-9223372036854775808ns", i64::MIN),
            ("-9223372036854775.808us", i64::MIN),
            ("-9223372036s854ms775us808ns", i64::MIN),
            ("-2562047h47m16.854775808s", i64::MIN),
            ("0.100000000000000000000h", 6 * 60 * S),
            (
                "0.830103483285477580700h",
                49 * 60 * S + 48 * S + 372_539_827,
            ),
        ];
        for &(text, want) in ok {
            assert_eq!(parse_duration(text), Some(want), "{text}");
        }
        for text in [
            "",
            "3",
            "-",
            "s",
            ".",
            "-.",
            ".s",
            "+.s",
            "1d",
            "\u{FFFD}",
            "9223372036854775808ns",
            "9223372036854775.808us",
            "9223372036854ms775us808ns",
            "-9223372036854775809ns",
            "3000000h",
            "9223372036854775807ns1ns",
            "18446744073709551616ns",
            "2562047h47m16.854775808s",
        ] {
            assert_eq!(parse_duration(text), None, "{text:?}");
        }
    }

    #[test]
    fn format_duration_matches_go() {
        for (nanos, want) in [
            (0, "0s"),
            (1, "1ns"),
            (1_100, "1.1µs"),
            (2_200 * 1_000, "2.2ms"),
            (3_300 * MS, "3.3s"),
            (4 * 60 * S + 5 * S, "4m5s"),
            (4 * 60 * S + 5001 * MS, "4m5.001s"),
            (5 * 3600 * S + 6 * 60 * S + 7001 * MS, "5h6m7.001s"),
            (8 * 60 * S + 1, "8m0.000000001s"),
            (i64::MAX, "2562047h47m16.854775807s"),
            (i64::MIN, "-2562047h47m16.854775808s"),
            (-S, "-1s"),
            (60 * S, "1m0s"),
            (3600 * S, "1h0m0s"),
            (24 * 3600 * S, "24h0m0s"),
            (10 * MS, "10ms"),
            (100 * MS, "100ms"),
        ] {
            assert_eq!(format_duration(nanos), want, "{nanos}");
            assert_eq!(parse_duration(want), Some(nanos), "{want}");
        }
    }

    #[test]
    fn split_host_port_matches_go() {
        for (input, host, port) in [
            ("localhost:http", "localhost", "http"),
            ("localhost:80", "localhost", "80"),
            ("[::1]:80", "::1", "80"),
            ("[fe80::1%lo0]:80", "fe80::1%lo0", "80"),
            (":80", "", "80"),
            ("127.0.0.1:", "127.0.0.1", ""),
            ("[]:0", "", "0"),
            ("a:", "a", ""),
        ] {
            assert_eq!(split_host_port(input), Ok((host, port)), "{input}");
        }
        for (input, err) in [
            ("golang.org", "missing port in address"),
            ("127.0.0.1", "missing port in address"),
            ("[::1]", "missing port in address"),
            ("[fe80::1%lo0]", "missing port in address"),
            ("[localhost%lo0]", "missing port in address"),
            ("localhost%lo0", "missing port in address"),
            ("::1", "too many colons in address"),
            ("fe80::1%lo0", "too many colons in address"),
            ("fe80::1%lo0:80", "too many colons in address"),
            ("[foo:bar]", "missing port in address"),
            ("[foo:bar]baz", "missing port in address"),
            ("[foo]bar:baz", "missing port in address"),
            ("[foo]:[bar]:baz", "too many colons in address"),
            ("[foo]:[bar]baz", "unexpected '[' in address"),
            ("foo[bar]:baz", "unexpected '[' in address"),
            ("foo]bar:baz", "unexpected ']' in address"),
            ("[::1", "missing ']' in address"),
        ] {
            assert_eq!(split_host_port(input), Err(err), "{input}");
        }
    }

    #[test]
    fn parse_addr_matches_go() {
        for ok in [
            "0.0.0.0",
            "192.168.140.255",
            "::",
            "::1",
            "fd7a:115c::626b:430b",
            "fd7a:115c:a1e0:ab12:4843:cd96:626b:430b",
            "::ffff:192.168.140.255",
            "::ffff:c0a8:8cff",
            "fe80::1%eth0",
            "fe80::1%%",
            "1:2:3:4:5:6:7::",
        ] {
            assert!(parse_addr(ok).is_some(), "{ok}");
        }
        assert_eq!(
            parse_addr("fe80::1%eth0").unwrap().zone.as_deref(),
            Some("eth0")
        );
        for bad in [
            "",
            "localhost",
            "010.0.0.1",
            "1.2.3",
            "1.2.3.4.5",
            "256.0.0.1",
            "1.2.3.4%eth0",
            "%eth0",
            "fe80::1%",
            "::ffff:01.2.3.4",
            "1:2:3:4:5:6:7:8:9",
            "12345::",
            ":::",
            "[::1]",
        ] {
            assert!(parse_addr(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn parse_prefix_matches_go() {
        let v4 = |s: &str| s.parse::<IpAddr>().unwrap();
        assert_eq!(parse_prefix("10.0.0.0/8"), Some((v4("10.0.0.0"), 8)));
        assert_eq!(parse_prefix("10.1.0.0/0"), Some((v4("10.1.0.0"), 0)));
        assert_eq!(
            parse_prefix("::ffff:1.2.3.0/120"),
            Some((v4("::ffff:1.2.3.0"), 120))
        );
        assert_eq!(parse_prefix("::/128"), Some((v4("::"), 128)));
        for bad in [
            "10.0.0.0",
            "10.0.0.0/",
            "10.0.0.0/33",
            "10.0.0.0/08",
            "10.0.0.0/+8",
            "10.0.0.0/-1",
            "::/129",
            "fe80::1%eth0/64",
            "x/8",
        ] {
            assert!(parse_prefix(bad).is_none(), "{bad}");
        }
        assert_eq!(mask(v4("10.1.2.3"), 8), v4("10.0.0.0"));
        assert_eq!(mask(v4("10.1.2.3"), 0), v4("0.0.0.0"));
        assert_eq!(mask(v4("10.1.2.3"), 32), v4("10.1.2.3"));
        assert_eq!(mask(v4("fd00::ff"), 120), v4("fd00::"));
        assert_eq!(mask(v4("fd00::ff"), 0), v4("::"));
    }

    #[test]
    fn lower_and_header_keys() {
        assert_eq!(to_lower("API.Example.COM"), "api.example.com");
        assert_eq!(to_lower("\u{130}.COM"), "i.com");
        assert_eq!(to_lower("\u{212A}.com"), "k.com");
        assert_eq!(to_lower("ΣΑΣ"), "σασ");
        assert_eq!(canonical_header_key("x-user-id"), "X-User-Id");
        assert_eq!(canonical_header_key("X-USER--ID"), "X-User--Id");
        assert_eq!(canonical_header_key("x_y.z~1"), "X_y.z~1");
    }

    #[test]
    fn paths_match_go() {
        for (input, want) in [
            ("", "."),
            ("abc", "abc"),
            ("abc/def", "abc/def"),
            ("a/b/c", "a/b/c"),
            (".", "."),
            ("..", ".."),
            ("../..", "../.."),
            ("../../abc", "../../abc"),
            ("/abc", "/abc"),
            ("/", "/"),
            ("abc/", "abc"),
            ("abc/def/", "abc/def"),
            ("abc//def//ghi", "abc/def/ghi"),
            ("//abc", "/abc"),
            ("abc/./def", "abc/def"),
            ("/./abc/def", "/abc/def"),
            ("abc/.", "abc"),
            ("abc/def/ghi/../jkl", "abc/def/jkl"),
            ("abc/def/../ghi/../jkl", "abc/jkl"),
            ("abc/def/..", "abc"),
            ("abc/def/../..", "."),
            ("/abc/def/../..", "/"),
            ("abc/def/../../..", ".."),
            ("/abc/def/../../..", "/"),
            ("abc/def/../../../ghi/jkl/../../../mno", "../../mno"),
            ("abc/./../def", "def"),
            ("abc//./../def", "def"),
            ("abc/../../././../def", "../../def"),
        ] {
            assert_eq!(clean(input), want, "clean({input:?})");
        }
        assert_eq!(dir("steer.json"), ".");
        assert_eq!(dir("/etc/steer.json"), "/etc");
        assert_eq!(dir("/steer.json"), "/");
        assert_eq!(dir("a/b/"), "a/b");
        assert_eq!(join(".", "a.crt"), "a.crt");
        assert_eq!(join("/etc/x", "../certs/a.crt"), "/etc/certs/a.crt");
        assert_eq!(join("", "a"), "a");
        assert_eq!(join("a", ""), "a");
        assert_eq!(join("", ""), "");
    }
}
