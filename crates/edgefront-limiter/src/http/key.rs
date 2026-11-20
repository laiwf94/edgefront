//! Keys from request fields: which bucket of a rule's table a request
//! takes from.
//!
//! A [`KeySpec`] is a list of [`Selector`]s, each reading one field of the
//! request:
//!
//! | selector | written | value |
//! |---|---|---|
//! | [`Selector::Path`] | `path` | the cleaned path the proxy routed on |
//! | [`Selector::PathPrefix`] | `path_prefix:/api` | the prefix, when the cleaned path is under it |
//! | [`Selector::Host`] | `host` | the host, lowercased, without port or trailing dot |
//! | [`Selector::Method`] | `method` | the method |
//! | [`Selector::Header`] | `header:x-api-key` | the header's value |
//! | [`Selector::Cookie`] | `cookie:session` | the cookie's value |
//! | [`Selector::Query`] | `query:api_key` | the query parameter's value, percent-decoded |
//!
//! A request with a selector's field absent (no such header, a path
//! outside the prefix) gets no key, and the rule does not apply to it, as
//! an Envoy descriptor with a missing entry is not limited.
//!
//! **Normalised, so a client cannot spell its way to a fresh bucket.** The
//! path is the one routing matched on: decoded and cleaned, so `/login`,
//! `/./login`, `//login` and `/%6Cogin` are one key. Hosts are lowercased
//! and lose their port and trailing dot. Query names and values are
//! percent-decoded with `+` as a space, so `api_key=%61bc` is `abc`.
//!
//! **Repeated fields share one bucket.** A header sent twice, a cookie
//! set twice or a query parameter given twice is ambiguous: the upstream
//! may read the first, the last or both, so keying on any one of them
//! would let a client send a fresh value beside the one the upstream
//! uses. Every request that repeats the field instead keys on a marker
//! that no single value encodes to, so all of them take from one bucket
//! per rule and the other fields.
//!
//! **No allocation.** The selectors' values are written into a [`Key`] on
//! the stack: each value byte as itself except 0, which is written `0 1`,
//! and each value ends with `0 0` (the repeat marker is `0 2`), so the
//! encoding is prefix-free and two different value lists never meet. A
//! key that fits in 31 bytes is stored as written. A longer one is hashed
//! as it is written, by two independently keyed SipHash-1-3 states, into
//! 32 bytes: 16 of hash and 16 zero. Every written key is shorter than 32,
//! so the two forms never meet either, and a collision of hashed keys
//! needs 128 bits of keyed hash to agree.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

use http::header::{COOKIE, HOST};
use http::{HeaderMap, HeaderName, Method, Uri};

use crate::table::KEY_MAX;

/// The longest key stored as written; longer ones are hashed.
pub const INLINE_MAX: usize = KEY_MAX - 1;

/// One field of the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selector {
    Path,
    /// The prefix itself, present only for paths under it: at the prefix
    /// or past a `/` after it, so `/api` covers `/api` and `/api/x` but
    /// not `/apix`. A prefix ending in `/` covers what starts with it.
    PathPrefix(String),
    Host,
    Method,
    /// Lowercase, as [`HeaderName`] keeps it.
    Header(HeaderName),
    Cookie(String),
    Query(String),
}

impl Selector {
    /// Reads `path`, `path_prefix:/p`, `host`, `method`, `header:NAME`,
    /// `cookie:NAME` or `query:NAME`.
    pub fn parse(text: &str) -> Result<Selector, String> {
        let (kind, arg) = match text.split_once(':') {
            Some((k, a)) => (k, Some(a)),
            None => (text, None),
        };
        let need = |what: &str| -> Result<&str, String> {
            match arg {
                Some(a) if !a.is_empty() => Ok(a),
                _ => Err(format!(
                    "{kind} needs a {what}, as in \"{kind}:{}\"",
                    example(kind)
                )),
            }
        };
        let none = |s: Selector| -> Result<Selector, String> {
            match arg {
                None => Ok(s),
                Some(_) => Err(format!("{kind} takes no argument")),
            }
        };
        match kind {
            "path" => none(Selector::Path),
            "host" => none(Selector::Host),
            "method" => none(Selector::Method),
            "path_prefix" => {
                let p = need("path")?;
                if !p.starts_with('/') {
                    return Err(format!("path_prefix {p:?} must start with /"));
                }
                let cleaned = edgefront_proxy::path::clean_path(p.as_bytes());
                if *cleaned != *p.as_bytes() {
                    return Err(format!(
                        "path_prefix {p:?} is not a clean path; write {:?}",
                        String::from_utf8_lossy(&cleaned)
                    ));
                }
                Ok(Selector::PathPrefix(p.to_owned()))
            }
            "header" => {
                let name = need("header name")?;
                HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes())
                    .map(Selector::Header)
                    .map_err(|_| format!("{name:?} is not a header name"))
            }
            "cookie" => {
                let name = need("cookie name")?;
                if !name.bytes().all(is_token) {
                    return Err(format!("{name:?} is not a cookie name"));
                }
                Ok(Selector::Cookie(name.to_owned()))
            }
            "query" => Ok(Selector::Query(need("parameter name")?.to_owned())),
            _ => Err(format!(
                "unknown key selector {text:?}; the selectors are path, path_prefix:/PREFIX, host, method, header:NAME, cookie:NAME and query:NAME"
            )),
        }
    }

    /// The selector as [`Selector::parse`] reads it.
    pub fn text(&self) -> String {
        match self {
            Selector::Path => "path".into(),
            Selector::PathPrefix(p) => format!("path_prefix:{p}"),
            Selector::Host => "host".into(),
            Selector::Method => "method".into(),
            Selector::Header(h) => format!("header:{h}"),
            Selector::Cookie(c) => format!("cookie:{c}"),
            Selector::Query(q) => format!("query:{q}"),
        }
    }
}

fn example(kind: &str) -> &'static str {
    match kind {
        "path_prefix" => "/api",
        "header" => "x-api-key",
        "cookie" => "session",
        _ => "api_key",
    }
}

/// RFC 9110 token characters, which cookie names are.
fn is_token(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// The request as the selectors read it.
#[derive(Clone, Copy, Debug)]
pub struct View<'a> {
    pub method: &'a Method,
    pub uri: &'a Uri,
    pub headers: &'a HeaderMap,
    /// The cleaned path, as routing matched it.
    pub path: &'a [u8],
}

impl View<'_> {
    /// The request's host: the URI's authority when it has one (absolute
    /// form, HTTP/2), the `Host` header otherwise; `None` when neither.
    pub fn host(&self) -> Option<&[u8]> {
        let raw = match self.uri.authority() {
            Some(a) => a.as_str().as_bytes(),
            None => self.headers.get(HOST)?.as_bytes(),
        };
        Some(host_part(raw))
    }
}

/// `raw` without user info, port and trailing dot. Not lowercased: the
/// caller compares or writes it ignoring case.
pub fn host_part(raw: &[u8]) -> &[u8] {
    let raw = match raw.iter().rposition(|&b| b == b'@') {
        Some(at) => &raw[at + 1..],
        None => raw,
    };
    let host = if raw.first() == Some(&b'[') {
        match raw.iter().position(|&b| b == b']') {
            Some(end) => &raw[..=end],
            None => raw,
        }
    } else {
        match raw.iter().rposition(|&b| b == b':') {
            Some(colon) => &raw[..colon],
            None => raw,
        }
    };
    host.strip_suffix(b".").unwrap_or(host)
}

/// Whether the cleaned `path` is under `prefix`.
pub fn under_prefix(path: &[u8], prefix: &[u8]) -> bool {
    match path.strip_prefix(prefix) {
        Some(rest) => rest.is_empty() || prefix.ends_with(b"/") || rest[0] == b'/',
        None => false,
    }
}

/// A key: up to [`KEY_MAX`] bytes on the stack.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    len: u8,
    bytes: [u8; KEY_MAX],
}

impl Key {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// Whether the key was hashed because the values did not fit.
    pub fn is_hashed(&self) -> bool {
        usize::from(self.len) == KEY_MAX
    }
}

impl std::fmt::Debug for Key {
    /// Never the bytes: a key may hold an API key or a session cookie.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Key")
            .field("len", &self.len)
            .field("hashed", &self.is_hashed())
            .finish()
    }
}

/// The two hash keys of one [`KeySpec`].
#[derive(Clone, Debug)]
struct Seeds(RandomState, RandomState);

/// Writes values into a key, hashing once they pass [`INLINE_MAX`].
struct Writer<'s> {
    seeds: &'s Seeds,
    buf: [u8; KEY_MAX],
    len: usize,
    hash: Option<(
        std::collections::hash_map::DefaultHasher,
        std::collections::hash_map::DefaultHasher,
    )>,
}

impl<'s> Writer<'s> {
    fn new(seeds: &'s Seeds) -> Self {
        Writer {
            seeds,
            buf: [0; KEY_MAX],
            len: 0,
            hash: None,
        }
    }

    #[inline]
    fn raw(&mut self, b: u8) {
        if let Some((a, c)) = &mut self.hash {
            a.write_u8(b);
            c.write_u8(b);
            return;
        }
        if self.len == INLINE_MAX {
            let mut a = self.seeds.0.build_hasher();
            let mut c = self.seeds.1.build_hasher();
            a.write(&self.buf[..self.len]);
            c.write(&self.buf[..self.len]);
            a.write_u8(b);
            c.write_u8(b);
            self.hash = Some((a, c));
            return;
        }
        self.buf[self.len] = b;
        self.len += 1;
    }

    #[inline]
    fn byte(&mut self, b: u8) {
        self.raw(b);
        if b == 0 {
            self.raw(1);
        }
    }

    fn bytes(&mut self, v: &[u8]) {
        for &b in v {
            self.byte(b);
        }
    }

    fn end(&mut self) {
        self.raw(0);
        self.raw(0);
    }

    /// The marker for a repeated field, in place of a value.
    fn repeated(&mut self) {
        self.raw(0);
        self.raw(2);
    }

    fn finish(self) -> Key {
        match self.hash {
            None => {
                let mut k = Key {
                    len: self.len as u8,
                    bytes: [0; KEY_MAX],
                };
                k.bytes[..self.len].copy_from_slice(&self.buf[..self.len]);
                k
            }
            Some((a, c)) => {
                let mut k = Key {
                    len: KEY_MAX as u8,
                    bytes: [0; KEY_MAX],
                };
                k.bytes[..8].copy_from_slice(&a.finish().to_le_bytes());
                k.bytes[8..16].copy_from_slice(&c.finish().to_le_bytes());
                k
            }
        }
    }
}

/// A field's value as found in the request.
enum Found<'a> {
    Absent,
    One(&'a [u8]),
    Repeated,
}

/// The selectors of one rule, with the hash keys its long keys use.
#[derive(Clone, Debug)]
pub struct KeySpec {
    selectors: Vec<Selector>,
    seeds: Seeds,
}

impl PartialEq for KeySpec {
    fn eq(&self, other: &Self) -> bool {
        self.selectors == other.selectors
    }
}

impl KeySpec {
    /// A spec for `selectors`, with fresh hash keys. An empty list gives
    /// every request the same key: one bucket for the whole rule.
    pub fn new(selectors: Vec<Selector>) -> Self {
        KeySpec {
            selectors,
            seeds: Seeds(RandomState::new(), RandomState::new()),
        }
    }

    pub fn selectors(&self) -> &[Selector] {
        &self.selectors
    }

    /// The request's key, or `None` when a selector's field is absent.
    pub fn key(&self, req: &View<'_>) -> Option<Key> {
        let mut w = Writer::new(&self.seeds);
        if self.selectors.is_empty() {
            // One key for every request; a table takes no empty key.
            w.end();
        }
        for s in &self.selectors {
            match s {
                Selector::Path => w.bytes(req.path),
                Selector::PathPrefix(p) => {
                    if !under_prefix(req.path, p.as_bytes()) {
                        return None;
                    }
                    w.bytes(p.as_bytes());
                }
                Selector::Host => {
                    for &b in req.host()? {
                        w.byte(b.to_ascii_lowercase());
                    }
                }
                Selector::Method => w.bytes(req.method.as_str().as_bytes()),
                Selector::Header(name) => {
                    let mut values = req.headers.get_all(name).iter();
                    let first = values.next()?;
                    if values.next().is_some() {
                        w.repeated();
                    } else {
                        w.bytes(first.as_bytes().trim_ascii());
                    }
                }
                Selector::Cookie(name) => match cookie(req.headers, name.as_bytes()) {
                    Found::Absent => return None,
                    Found::One(v) => w.bytes(v),
                    Found::Repeated => w.repeated(),
                },
                Selector::Query(name) => match query(req.uri.query(), name.as_bytes()) {
                    Found::Absent => return None,
                    Found::One(v) => {
                        for b in Decode(v) {
                            w.byte(b);
                        }
                    }
                    Found::Repeated => w.repeated(),
                },
            }
            w.end();
        }
        Some(w.finish())
    }
}

/// The cookie `name` across every `Cookie` header, as RFC 6265 section
/// 4.2 lays them out: `name=value` pairs split by `;`. A value in double
/// quotes loses them.
fn cookie<'a>(headers: &'a HeaderMap, name: &[u8]) -> Found<'a> {
    let mut found = Found::Absent;
    for value in headers.get_all(COOKIE) {
        for pair in value.as_bytes().split(|&b| b == b';') {
            let pair = pair.trim_ascii();
            let Some(eq) = pair.iter().position(|&b| b == b'=') else {
                continue;
            };
            if pair[..eq].trim_ascii() != name {
                continue;
            }
            if !matches!(found, Found::Absent) {
                return Found::Repeated;
            }
            let v = pair[eq + 1..].trim_ascii();
            let v = v
                .strip_prefix(b"\"")
                .and_then(|v| v.strip_suffix(b"\""))
                .unwrap_or(v);
            found = Found::One(v);
        }
    }
    found
}

/// The query parameter `name`, compared after decoding, with its raw
/// value.
fn query<'a>(q: Option<&'a str>, name: &[u8]) -> Found<'a> {
    let Some(q) = q else {
        return Found::Absent;
    };
    let mut found = Found::Absent;
    for pair in q.as_bytes().split(|&b| b == b'&') {
        let (n, v) = match pair.iter().position(|&b| b == b'=') {
            Some(eq) => (&pair[..eq], &pair[eq + 1..]),
            None => (pair, &b""[..]),
        };
        if !Decode(n).eq(name.iter().copied()) {
            continue;
        }
        if !matches!(found, Found::Absent) {
            return Found::Repeated;
        }
        found = Found::One(v);
    }
    found
}

/// Percent-decoding as forms do it: `%XX` is the byte, `+` a space, and a
/// `%` not followed by two hex digits stays itself.
struct Decode<'a>(&'a [u8]);

impl Iterator for Decode<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        let (&b, rest) = self.0.split_first()?;
        if b == b'%'
            && let [h, l, tail @ ..] = rest
            && let (Some(h), Some(l)) = (hex(*h), hex(*l))
        {
            self.0 = tail;
            return Some(h << 4 | l);
        }
        self.0 = rest;
        Some(if b == b'+' { b' ' } else { b })
    }
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
