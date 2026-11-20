//! The real client behind proxies: the address to limit when the peer is
//! a proxy we trust, read from `Forwarded` (RFC 7239) or
//! `X-Forwarded-For`.
//!
//! Each proxy appends the address it received the request from, so the
//! list reads client first and nearest proxy last. Anyone can send these
//! headers, so only what our own trusted proxies appended counts. The
//! rule is rightmost-untrusted: start at the peer; while the address in
//! hand is a trusted proxy and the list has an entry left, take the
//! rightmost remaining entry. The first address that is not trusted is
//! the client. A client that sends `X-Forwarded-For: 1.2.3.4` itself only
//! adds an entry left of the one our proxy appended, which is never
//! reached while that entry is not a trusted proxy.
//!
//! - A peer that is not trusted is the client, whatever the headers say.
//! - When every entry is trusted (a request from one of our own proxies),
//!   the leftmost entry is the client.
//! - An entry that is not an address (`unknown`, an obfuscated node, a
//!   malformed value) stops the walk: the client is the trusted proxy that
//!   appended it, and [`Resolved::malformed`] says why. Limiting that
//!   proxy's address is conservative: nothing a client sends can choose
//!   another client's key.
//! - Several header lines are one list, in order.
//!
//! Only one header is read, the one configured ([`Header`]): with both,
//! a client could put an address in whichever the proxy does not touch.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::cidr::{PrefixList, canonical};

/// Which header carries the chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Header {
    /// RFC 7239 `Forwarded: for=...`.
    Forwarded,
    /// `X-Forwarded-For: a, b, c`.
    XForwardedFor,
}

impl Header {
    /// The header's name, lower case.
    pub const fn name(self) -> &'static str {
        match self {
            Header::Forwarded => "forwarded",
            Header::XForwardedFor => "x-forwarded-for",
        }
    }
}

/// The client found, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The address to limit.
    pub client: IpAddr,
    /// How many header entries were taken: 0 when the peer is the client.
    pub hops: usize,
    /// Why the walk stopped at a trusted proxy, when it did.
    pub malformed: Option<String>,
}

/// One `X-Forwarded-For` entry: an address, an IPv6 address in brackets,
/// either with a port.
fn xff_entry(s: &str) -> Result<IpAddr, String> {
    let t = s.trim();
    if let Ok(a) = t.parse::<IpAddr>() {
        return Ok(a);
    }
    if let Some(rest) = t.strip_prefix('[')
        && let Some((a, port)) = rest.split_once(']')
        && (port.is_empty() || port.strip_prefix(':').is_some_and(valid_port))
    {
        return a.parse::<Ipv6Addr>().map(IpAddr::V6).map_err(|_| bad(t));
    }
    if let Some((a, port)) = t.rsplit_once(':')
        && valid_port(port)
        && let Ok(v4) = a.parse::<Ipv4Addr>()
    {
        return Ok(IpAddr::V4(v4));
    }
    Err(bad(t))
}

fn bad(t: &str) -> String {
    let shown: String = t.chars().take(64).collect();
    format!("{shown:?} is not an address")
}

fn valid_port(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 5
        && p.bytes().all(|b| b.is_ascii_digit())
        && p.parse::<u16>().is_ok()
}

/// The `for=` node of one RFC 7239 element. The node is a token or a
/// quoted string: an IPv4 address, a bracketed IPv6 address (which must
/// be quoted), either with an optional port, `unknown` or an obfuscated
/// `_name`. Returns `Ok(None)` when the element has no `for=`.
fn forwarded_for(element: &str) -> Result<Option<IpAddr>, String> {
    let mut found = None;
    for pair in split_outside_quotes(element, ';') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let Some((k, v)) = pair.split_once('=') else {
            return Err(format!(
                "{:?} is not a parameter",
                pair.chars().take(64).collect::<String>()
            ));
        };
        if !k.trim().eq_ignore_ascii_case("for") {
            continue;
        }
        if found.is_some() {
            return Err("for= appears twice in one element".into());
        }
        let v = v.trim();
        let node = match v.strip_prefix('"') {
            Some(q) => {
                let inner = q
                    .strip_suffix('"')
                    .ok_or_else(|| format!("{v:?} has an unterminated quote"))?;
                if inner.contains(['"', '\\']) {
                    return Err(format!("{v:?} has an escape, which no address needs"));
                }
                inner
            }
            None => {
                // A token cannot hold the brackets or colon of an IPv6 node.
                if v.contains(['[', ']', '"']) {
                    return Err(format!("{v:?}: an IPv6 node must be quoted"));
                }
                v
            }
        };
        if node.eq_ignore_ascii_case("unknown") {
            return Err("for=unknown".into());
        }
        if node.starts_with('_') {
            return Err(format!("for={node:?} is an obfuscated node"));
        }
        let ip = if let Some(rest) = node.strip_prefix('[') {
            let (a, port) = rest
                .split_once(']')
                .ok_or_else(|| format!("{node:?} has no closing bracket"))?;
            if !(port.is_empty()
                || port
                    .strip_prefix(':')
                    .is_some_and(|p| valid_port(p) || p.starts_with('_')))
            {
                return Err(bad(node));
            }
            IpAddr::V6(a.parse::<Ipv6Addr>().map_err(|_| bad(node))?)
        } else {
            let a = match node.rsplit_once(':') {
                Some((a, p)) if valid_port(p) || p.starts_with('_') => a,
                _ => node,
            };
            IpAddr::V4(a.parse::<Ipv4Addr>().map_err(|_| bad(node))?)
        };
        found = Some(ip);
    }
    Ok(found)
}

/// Splits on `sep` where it is not inside a quoted string.
fn split_outside_quotes(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            c if c == sep && !quoted => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// The chain one header carries, client first: each entry an address or
/// the reason it is not one.
pub fn chain(header: Header, values: &[&str]) -> Vec<Result<IpAddr, String>> {
    let mut out = Vec::new();
    for v in values {
        match header {
            Header::XForwardedFor => {
                for e in v.split(',') {
                    if e.trim().is_empty() {
                        continue;
                    }
                    out.push(xff_entry(e));
                }
            }
            Header::Forwarded => {
                for e in split_outside_quotes(v, ',') {
                    if e.trim().is_empty() {
                        continue;
                    }
                    match forwarded_for(e) {
                        Ok(Some(ip)) => out.push(Ok(ip)),
                        // An element without for= says nothing about the
                        // client; it is not a hop.
                        Ok(None) => {}
                        Err(why) => out.push(Err(why)),
                    }
                }
            }
        }
    }
    out
}

/// The client behind `peer`, trusting only the proxies in `trusted`, from
/// the values of `header` in the request, in order. See the
/// [module docs](self).
pub fn resolve(
    peer: IpAddr,
    trusted: &PrefixList<()>,
    header: Header,
    values: &[&str],
) -> Resolved {
    let mut client = canonical(peer);
    let mut hops = 0;
    if !trusted.contains(client) {
        return Resolved {
            client,
            hops,
            malformed: None,
        };
    }
    let entries = chain(header, values);
    for e in entries.iter().rev() {
        match e {
            Ok(a) => {
                client = canonical(*a);
                hops += 1;
                if !trusted.contains(client) {
                    break;
                }
            }
            Err(why) => {
                return Resolved {
                    client,
                    hops,
                    malformed: Some(why.clone()),
                };
            }
        }
    }
    Resolved {
        client,
        hops,
        malformed: None,
    }
}

#[cfg(test)]
mod tests;
