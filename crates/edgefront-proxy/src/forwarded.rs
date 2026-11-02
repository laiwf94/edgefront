//! The client's address and the `X-Forwarded-*` headers, ported from
//! steer's `internal/proxy/forwarded.go`.
//!
//! Forwarding headers are only believed from a trusted proxy (a peer inside
//! `trusted_proxies`). From anyone else they are dropped and replaced with
//! what this hop saw itself, so a client cannot claim another address,
//! scheme or host. From a trusted proxy the inbound values are carried over,
//! but only in well-formed shape: `X-Forwarded-For` keeps the entries that
//! are IP addresses (at most the last [`MAX_FORWARDED_HOPS`]), and
//! `X-Forwarded-Proto` and `X-Forwarded-Host` are kept only when valid.
//!
//! Go's address syntax decides what counts as an address (`netip`: no
//! leading zeros in IPv4, a non-empty zone), Go's `strings.TrimSpace`
//! decides what surrounds one, and addresses are written as Go writes them,
//! so the headers are byte-identical to steer's for the same input.

use std::net::IpAddr;

use edgefront_core::config::IpPrefix;
use edgefront_core::config::go::{parse_addr, split_host_port};
use http::HeaderMap;
use http::header::{FORWARDED, HeaderName, HeaderValue};

/// How many entries of a trusted `X-Forwarded-For` chain are kept: the last
/// ones, nearest to this hop.
pub const MAX_FORWARDED_HOPS: usize = 32;

/// `X-Forwarded-For`.
pub const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
/// `X-Forwarded-Proto`.
pub const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
/// `X-Forwarded-Host`.
pub const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");

/// The address of a connection's peer from Go's `RemoteAddr` form
/// (`host:port`, `[v6]:port`, or a bare address), as steer's `peerAddr`
/// reads it: IPv4-mapped IPv6 becomes IPv4 and any zone is dropped.
pub fn peer_addr(remote: &str) -> Option<IpAddr> {
    let host = split_host_port(remote).map_or(remote, |(host, _)| host);
    parse_addr(host).map(|a| a.ip.to_canonical())
}

/// Whether `ip` is inside one of the `trusted` networks.
pub fn is_trusted(ip: IpAddr, trusted: &[IpPrefix]) -> bool {
    trusted.iter().any(|p| p.contains(ip))
}

/// The addresses in a set of `X-Forwarded-For` values, left to right.
/// Entries that are not IP addresses are dropped, IPv4-mapped addresses
/// become IPv4, zones are dropped, and only the last
/// [`MAX_FORWARDED_HOPS`] are kept.
pub fn forwarded_chain<'v, I>(values: I) -> Vec<IpAddr>
where
    I: IntoIterator<Item = &'v [u8]>,
{
    let mut out = Vec::new();
    for value in values {
        for part in value.split(|&c| c == b',') {
            let addr = std::str::from_utf8(trim_space(part))
                .ok()
                .and_then(parse_addr);
            if let Some(a) = addr {
                out.push(a.ip.to_canonical());
            }
        }
    }
    if out.len() > MAX_FORWARDED_HOPS {
        out.drain(..out.len() - MAX_FORWARDED_HOPS);
    }
    out
}

/// steer's `ClientIP`: the client's address as far as it can be trusted.
///
/// That is the peer, unless the peer is a trusted proxy; then it is the
/// rightmost `X-Forwarded-For` entry that is not itself trusted (entries to
/// its left could have been written by anyone), or the leftmost entry when
/// all are trusted, or the peer when there are none. `None` when the peer
/// is unknown.
pub fn client_ip<'v, I>(peer: Option<IpAddr>, xff: I, trusted: &[IpPrefix]) -> Option<IpAddr>
where
    I: IntoIterator<Item = &'v [u8]>,
{
    let peer = peer?;
    if !is_trusted(peer, trusted) {
        return Some(peer);
    }
    Some(pick_client(peer, &forwarded_chain(xff), trusted))
}

fn pick_client(peer: IpAddr, chain: &[IpAddr], trusted: &[IpPrefix]) -> IpAddr {
    chain
        .iter()
        .rev()
        .find(|&&a| !is_trusted(a, trusted))
        .or(chain.first())
        .copied()
        .unwrap_or(peer)
}

/// Rewrites a request's forwarding headers for the upstream, as steer did
/// with `httputil.ReverseProxy` and `setForwarded`, and returns the
/// client's address as [`client_ip`] gives it.
///
/// `headers` are the inbound request's headers; afterwards they hold what
/// steer sent upstream:
///
/// - `Forwarded` is removed (steer neither trusts nor writes it).
/// - `X-Forwarded-For` is the trusted inbound chain, if the peer is a
///   trusted proxy, followed by the peer; absent if the peer is unknown.
/// - `X-Forwarded-Proto` is `https` when `tls`, else `http`, unless a
///   trusted peer sent `http` or `https` in any case.
/// - `X-Forwarded-Host` is a trusted peer's valid `X-Forwarded-Host`, else
///   `host` (the request's Host); absent if neither is valid (see
///   [`valid_host_header`]).
pub fn set_forwarded(
    headers: &mut HeaderMap,
    peer: Option<IpAddr>,
    tls: bool,
    host: &[u8],
    trusted: &[IpPrefix],
) -> Option<IpAddr> {
    let from_trusted = peer.is_some_and(|p| is_trusted(p, trusted));

    let chain = if from_trusted {
        forwarded_chain(
            headers
                .get_all(&X_FORWARDED_FOR)
                .iter()
                .map(HeaderValue::as_bytes),
        )
    } else {
        Vec::new()
    };
    let client = peer.map(|p| {
        if from_trusted {
            pick_client(p, &chain, trusted)
        } else {
            p
        }
    });

    let mut proto: &'static str = if tls { "https" } else { "http" };
    let mut fwd_host: Option<HeaderValue> = None;
    if from_trusted {
        if let Some(p) = headers.get(&X_FORWARDED_PROTO) {
            if p.as_bytes().eq_ignore_ascii_case(b"http") {
                proto = "http";
            } else if p.as_bytes().eq_ignore_ascii_case(b"https") {
                proto = "https";
            }
        }
        fwd_host = headers
            .get(&X_FORWARDED_HOST)
            .filter(|h| valid_host_header(h.as_bytes()))
            .cloned();
    }
    if fwd_host.is_none() && valid_host_header(host) {
        fwd_host = HeaderValue::from_bytes(host).ok();
    }

    headers.remove(FORWARDED);
    headers.remove(&X_FORWARDED_FOR);
    headers.remove(&X_FORWARDED_PROTO);
    headers.remove(&X_FORWARDED_HOST);

    if let Some(peer) = peer {
        let mut xff = String::with_capacity(16 * (chain.len() + 1));
        for a in chain.iter().chain(std::iter::once(&peer)) {
            if !xff.is_empty() {
                xff.push_str(", ");
            }
            // Writing to a String cannot fail.
            let _ = std::fmt::Write::write_fmt(&mut xff, format_args!("{a}"));
        }
        let value = HeaderValue::try_from(xff).expect("addresses are visible ASCII");
        headers.insert(X_FORWARDED_FOR, value);
    }
    headers.insert(X_FORWARDED_PROTO, HeaderValue::from_static(proto));
    if let Some(h) = fwd_host {
        headers.insert(X_FORWARDED_HOST, h);
    }
    client
}

/// steer's `validHostHeader`: 1 to 255 bytes of printable ASCII, without
/// spaces or commas.
pub fn valid_host_header(h: &[u8]) -> bool {
    (1..=255).contains(&h.len()) && h.iter().all(|&c| c > b' ' && c < 0x7f && c != b',')
}

/// Go's `strings.TrimSpace` on bytes: trims Unicode white space from both
/// ends, where a byte sequence that is not valid UTF-8 is not white space.
fn trim_space(mut s: &[u8]) -> &[u8] {
    while let Some((c, len)) = first_char(s) {
        if !c.is_whitespace() {
            break;
        }
        s = &s[len..];
    }
    while let Some((c, len)) = last_char(s) {
        if !c.is_whitespace() {
            break;
        }
        s = &s[..s.len() - len];
    }
    s
}

/// The first character of `s` and its length, if `s` starts with valid
/// UTF-8.
fn first_char(s: &[u8]) -> Option<(char, usize)> {
    let len = match *s.first()? {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return None,
    };
    let c = std::str::from_utf8(s.get(..len)?).ok()?.chars().next()?;
    Some((c, len))
}

/// The last character of `s` and its length, if `s` ends with valid UTF-8
/// (Go's `utf8.DecodeLastRune` without the error rune).
fn last_char(s: &[u8]) -> Option<(char, usize)> {
    let start = s
        .iter()
        .rev()
        .take(4)
        .position(|&c| c & 0xc0 != 0x80)
        .map(|back| s.len() - 1 - back)?;
    let (c, len) = first_char(&s[start..])?;
    (start + len == s.len()).then_some((c, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefixes(ss: &[&str]) -> Vec<IpPrefix> {
        ss.iter().map(|s| s.parse().unwrap()).collect()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// steer's `TestClientIP`, case for case.
    #[test]
    fn steer_client_ip_vectors() {
        let trusted = prefixes(&["10.0.0.0/8", "::1/128"]);
        let cases: &[(&str, &[&str], &str)] = &[
            ("203.0.113.5:4000", &["1.1.1.1"], "203.0.113.5"),
            ("10.0.0.2:4000", &["1.1.1.1, 10.0.0.9"], "1.1.1.1"),
            ("10.0.0.2:4000", &["6.6.6.6, 2.2.2.2"], "2.2.2.2"),
            ("10.0.0.2:4000", &["10.0.0.3"], "10.0.0.3"),
            ("10.0.0.2:4000", &[], "10.0.0.2"),
            ("10.0.0.2:4000", &["garbage, 3.3.3.3, nope"], "3.3.3.3"),
            ("[::1]:4000", &["2001:db8::1"], "2001:db8::1"),
            ("[::ffff:10.0.0.2]:1", &["4.4.4.4"], "4.4.4.4"),
            ("not-an-address", &["4.4.4.4"], ""),
            ("10.0.0.2", &["5.5.5.5"], "5.5.5.5"),
            ("10.0.0.2:1", &["fe80::1%eth0"], "fe80::1"),
        ];
        for &(remote, xff, want) in cases {
            let got = client_ip(
                peer_addr(remote),
                xff.iter().map(|v| v.as_bytes()),
                &trusted,
            )
            .map(|a| a.to_string())
            .unwrap_or_default();
            assert_eq!(got, want, "{remote} {xff:?}");
        }
    }

    /// steer's `TestForwardedChainIsBounded`.
    #[test]
    fn forwarded_chain_is_bounded() {
        let parts: Vec<String> = (0..100).map(|i| format!("1.1.1.{i}")).collect();
        let header = parts.join(",");
        let got = forwarded_chain([header.as_bytes()]);
        assert_eq!(got.len(), MAX_FORWARDED_HOPS);
        assert_eq!(got[0], ip("1.1.1.68"));
        assert_eq!(got[MAX_FORWARDED_HOPS - 1], ip("1.1.1.99"));
    }

    /// steer's `TestValidHostHeader`.
    #[test]
    fn steer_valid_host_header_vectors() {
        for ok in ["a.com", "a.com:8080", "[::1]:80"] {
            assert!(valid_host_header(ok.as_bytes()), "{ok:?} rejected");
        }
        let long = "a".repeat(256);
        for bad in ["", "a b", "a\r\nb", "a,b", &long, "é.com"] {
            assert!(!valid_host_header(bad.as_bytes()), "{bad:?} accepted");
        }
        assert!(valid_host_header("a".repeat(255).as_bytes()));
        assert!(!valid_host_header(b"a\x7f"));
    }

    #[test]
    fn peer_addresses_follow_go() {
        assert_eq!(peer_addr("1.2.3.4:5"), Some(ip("1.2.3.4")));
        assert_eq!(peer_addr("[::ffff:1.2.3.4]:5"), Some(ip("1.2.3.4")));
        assert_eq!(peer_addr("[fe80::1%eth0]:5"), Some(ip("fe80::1")));
        assert_eq!(peer_addr("::1"), Some(ip("::1")));
        assert_eq!(peer_addr("1.2.3.4"), Some(ip("1.2.3.4")));
        for bad in [
            "",
            "[::1]",
            "010.0.0.1:1",
            "host:80",
            "[::1]:80:90",
            "1.2.3.4:",
        ] {
            let want = (bad == "1.2.3.4:").then(|| ip("1.2.3.4"));
            assert_eq!(peer_addr(bad), want, "{bad:?}");
        }
    }

    #[test]
    fn trim_space_follows_go() {
        let cases: &[(&[u8], &[u8])] = &[
            (b" \t1.2.3.4\r\n", b"1.2.3.4"),
            ("\u{a0}1.2.3.4\u{2003}".as_bytes(), b"1.2.3.4"),
            ("\u{85}\u{3000}x\u{2029}".as_bytes(), b"x"),
            (b"\x85x", b"\x85x"),
            (b"x\x85", b"x\x85"),
            (b"x\xc2", b"x\xc2"),
            (b"\xe2\x80", b"\xe2\x80"),
            (b"\xff ", b"\xff"),
            (b"   ", b""),
            (b"", b""),
            ("é ".as_bytes(), "é".as_bytes()),
            (b" \x80\x80\x80\x80", b"\x80\x80\x80\x80"),
        ];
        for &(input, want) in cases {
            assert_eq!(trim_space(input), want, "{input:?}");
        }
    }

    fn headers(pairs: &[(&HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (name, value) in pairs {
            h.append(*name, HeaderValue::from_str(value).unwrap());
        }
        h
    }

    fn get<'a>(h: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
        assert!(h.get_all(name).iter().count() <= 1, "{name} repeated");
        h.get(name).map(|v| v.to_str().unwrap())
    }

    /// The header half of steer's `TestForwardProxyHeaders`: an untrusted
    /// client's forwarding headers are replaced.
    #[test]
    fn untrusted_client_headers_are_replaced() {
        let mut h = headers(&[
            (&X_FORWARDED_FOR, "6.6.6.6"),
            (&X_FORWARDED_PROTO, "https"),
            (&X_FORWARDED_HOST, "evil.example"),
            (&FORWARDED, "for=6.6.6.6"),
        ]);
        let client = set_forwarded(
            &mut h,
            peer_addr("127.0.0.1:5555"),
            false,
            b"shop.example.com",
            &[],
        );
        assert_eq!(client, Some(ip("127.0.0.1")));
        assert_eq!(get(&h, &X_FORWARDED_FOR), Some("127.0.0.1"));
        assert_eq!(get(&h, &X_FORWARDED_PROTO), Some("http"));
        assert_eq!(get(&h, &X_FORWARDED_HOST), Some("shop.example.com"));
        assert_eq!(get(&h, &FORWARDED), None);
    }

    /// steer's `TestTrustedProxyChain`.
    #[test]
    fn trusted_proxy_chain() {
        let trusted = prefixes(&["127.0.0.0/8"]);
        let peer = peer_addr("127.0.0.1:5555");
        let mut h = headers(&[
            (&X_FORWARDED_FOR, "203.0.113.9, junk"),
            (&X_FORWARDED_FOR, "10.1.1.1"),
            (&X_FORWARDED_PROTO, "HTTPS"),
            (&X_FORWARDED_HOST, "www.example.com"),
        ]);
        let client = set_forwarded(&mut h, peer, false, b"127.0.0.1:8080", &trusted);
        assert_eq!(client, Some(ip("10.1.1.1")));
        assert_eq!(
            get(&h, &X_FORWARDED_FOR),
            Some("203.0.113.9, 10.1.1.1, 127.0.0.1")
        );
        assert_eq!(get(&h, &X_FORWARDED_PROTO), Some("https"));
        assert_eq!(get(&h, &X_FORWARDED_HOST), Some("www.example.com"));

        // Malformed proto and host from a trusted proxy fall back to our own
        // view.
        let mut h = headers(&[(&X_FORWARDED_PROTO, "gopher"), (&X_FORWARDED_HOST, "a b")]);
        let client = set_forwarded(&mut h, peer, true, b"127.0.0.1:8080", &trusted);
        assert_eq!(client, peer);
        assert_eq!(get(&h, &X_FORWARDED_FOR), Some("127.0.0.1"));
        assert_eq!(get(&h, &X_FORWARDED_PROTO), Some("https"));
        assert_eq!(get(&h, &X_FORWARDED_HOST), Some("127.0.0.1:8080"));

        let mut h = headers(&[(&X_FORWARDED_PROTO, "Http")]);
        set_forwarded(&mut h, peer, true, b"", &trusted);
        assert_eq!(get(&h, &X_FORWARDED_PROTO), Some("http"));
        assert_eq!(get(&h, &X_FORWARDED_HOST), None);
    }

    #[test]
    fn unknown_peer_writes_no_chain() {
        let mut h = headers(&[(&X_FORWARDED_FOR, "1.1.1.1")]);
        let client = set_forwarded(&mut h, peer_addr("unix-socket"), true, b"a.com", &[]);
        assert_eq!(client, None);
        assert_eq!(get(&h, &X_FORWARDED_FOR), None);
        assert_eq!(get(&h, &X_FORWARDED_PROTO), Some("https"));
        assert_eq!(get(&h, &X_FORWARDED_HOST), Some("a.com"));
        assert_eq!(client_ip(None, [&b"1.1.1.1"[..]], &[]), None);
    }
}
