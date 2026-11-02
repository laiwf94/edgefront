//! Request paths as steer sees them: how a request target is decoded,
//! cleaned for routing and rewritten for the upstream.
//!
//! steer let Go's `net/url` parse the request target, routed on
//! `route.CleanPath` of the decoded path, and sent the upstream either the
//! client's own encoding (when cleaning changed nothing) or Go's encoding of
//! the cleaned path. This module ports each of those steps so that the same
//! bytes give the same route and the same upstream request line:
//!
//! - **Decoding** (`url.ParseRequestURI`): control bytes and malformed
//!   `%` escapes are rejected; every other escape is decoded, so `%2F`
//!   becomes `/` and `%2e%2e` becomes `..` before cleaning. `+` stays `+`.
//! - **Cleaning** (`route.CleanPath`): dot segments are resolved, repeated
//!   slashes collapse, `..` never climbs above the root, and a trailing
//!   slash is kept. Because decoding comes first, an encoded separator
//!   (`/public%2F..%2Fadmin`) is cleaned like a literal one and routes as
//!   `/admin`.
//! - **Re-encoding** (`URL.EscapedPath` and `URL.RequestURI`): when
//!   cleaning changed the path, the upstream gets Go's encoding of the
//!   cleaned path. When it did not, the upstream gets the client's bytes
//!   if they are a valid encoding (`/a%2Fb` stays `/a%2Fb`), and Go's
//!   encoding of the decoded path otherwise (`/a b` becomes `/a%20b`). The
//!   query is passed through untouched.
//!
//! Paths are bytes, not `str`: a Go string may hold any bytes, and `%ff`
//! decodes to a lone `0xff` that has to route and re-encode the same way.

use std::borrow::Cow;
use std::fmt;

/// Why a request target was refused. steer's Go server answered each of
/// these with `400 Bad Request` before routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetError {
    /// The target is empty.
    Empty,
    /// The target holds a control byte (below `0x20`, or `0x7f`).
    ControlByte,
    /// The target is neither `*` nor a path starting with `/`.
    NotOriginForm,
    /// A `%` is not followed by two hex digits. Holds the offending text,
    /// at most three bytes from the `%`.
    InvalidEscape(Vec<u8>),
}

impl fmt::Display for TargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("empty request target"),
            Self::ControlByte => f.write_str("invalid control character in request target"),
            Self::NotOriginForm => f.write_str("invalid URI for request"),
            Self::InvalidEscape(s) => {
                write!(f, "invalid URL escape {:?}", String::from_utf8_lossy(s))
            }
        }
    }
}

impl std::error::Error for TargetError {}

/// An origin-form request target (`/path?query`, or `*`) after decoding
/// and cleaning, borrowing from the target where it can.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestTarget<'a> {
    /// The whole target as received.
    target: &'a [u8],
    /// The path part of `target`, still encoded (Go's `RawPath` source).
    raw_path: &'a [u8],
    /// Everything after the first `?`, if there is one.
    query: Option<&'a [u8]>,
    /// The decoded path (Go's `URL.Path`).
    path: Cow<'a, [u8]>,
    /// The cleaned path, when cleaning changed it.
    cleaned: Option<Vec<u8>>,
}

impl<'a> RequestTarget<'a> {
    /// Parses a request target the way Go's `url.ParseRequestURI` does for
    /// one in origin form, and cleans its path.
    ///
    /// For an absolute-form target (`http://host/path?q`), pass the path
    /// and query, which is what Go parses after the authority.
    pub fn parse(target: &'a [u8]) -> Result<Self, TargetError> {
        if target.iter().any(|&c| c < b' ' || c == 0x7f) {
            return Err(TargetError::ControlByte);
        }
        if target.is_empty() {
            return Err(TargetError::Empty);
        }
        let (raw_path, query) = if target == b"*" {
            (target, None)
        } else {
            let (raw_path, query) = match target.iter().position(|&c| c == b'?') {
                Some(at) => (&target[..at], Some(&target[at + 1..])),
                None => (target, None),
            };
            if raw_path.first() != Some(&b'/') {
                return Err(TargetError::NotOriginForm);
            }
            (raw_path, query)
        };
        let path = unescape(raw_path)?;
        let cleaned = match clean_path(&path) {
            Cow::Borrowed(_) => None,
            Cow::Owned(c) => Some(c),
        };
        Ok(Self {
            target,
            raw_path,
            query,
            path,
            cleaned,
        })
    }

    /// The decoded path, before cleaning (Go's `URL.Path`).
    pub fn path(&self) -> &[u8] {
        &self.path
    }

    /// The cleaned path that routing matches on (steer's
    /// `route.CleanPath(r.URL.Path)`).
    pub fn clean_path(&self) -> &[u8] {
        self.cleaned.as_deref().unwrap_or(&self.path)
    }

    /// Whether cleaning changed the path.
    pub fn was_cleaned(&self) -> bool {
        self.cleaned.is_some()
    }

    /// The raw query: everything after the first `?`, possibly empty.
    /// `None` when the target has no `?`.
    pub fn query(&self) -> Option<&[u8]> {
        self.query
    }

    /// The path as Go's `URL.EscapedPath` gives it for the parsed target:
    /// the client's encoding when it is a valid one, Go's otherwise.
    pub fn escaped_path(&self) -> Cow<'a, [u8]> {
        if valid_encoded(self.raw_path) {
            Cow::Borrowed(self.raw_path)
        } else if *self.path == *b"*" {
            Cow::Borrowed(b"*")
        } else {
            Cow::Owned(escape(&self.path))
        }
    }

    /// The request target steer sent upstream: the cleaned path in Go's
    /// encoding if cleaning changed it, [`RequestTarget::escaped_path`]
    /// otherwise, then `?` and the query if the client sent a `?`. When
    /// nothing needs re-encoding this is the client's target itself.
    pub fn upstream_target(&self) -> Cow<'a, [u8]> {
        let mut out = match &self.cleaned {
            Some(cleaned) => escape(cleaned),
            // The path goes as the client sent it, and the query always
            // does, so the target does too.
            None if valid_encoded(self.raw_path) => return Cow::Borrowed(self.target),
            None => self.escaped_path().into_owned(),
        };
        if let Some(query) = self.query {
            out.reserve(1 + query.len());
            out.push(b'?');
            out.extend_from_slice(query);
        }
        Cow::Owned(out)
    }
}

/// steer's `route.CleanPath`: Go's `path.Clean` of the path made absolute,
/// keeping a trailing slash. The result starts with `/`, holds no `.` or
/// `..` segment and no repeated slash, and cleaning it again changes
/// nothing. A path that is already clean is returned as it is.
pub fn clean_path(p: &[u8]) -> Cow<'_, [u8]> {
    if is_clean(p) {
        return Cow::Borrowed(p);
    }
    let mut out = Vec::with_capacity(p.len() + 1);
    for segment in p.split(|&c| c == b'/') {
        match segment {
            b"" | b"." => {}
            b".." => {
                // Every segment in `out` starts with its slash, so dropping
                // back to the last slash drops one segment, and an empty
                // `out` is the root, which `..` cannot leave.
                let last = out.iter().rposition(|&c| c == b'/').unwrap_or(0);
                out.truncate(last);
            }
            _ => {
                out.push(b'/');
                out.extend_from_slice(segment);
            }
        }
    }
    if out.is_empty() || p.ends_with(b"/") {
        out.push(b'/');
    }
    Cow::Owned(out)
}

/// Whether [`clean_path`] would return `p` unchanged.
fn is_clean(p: &[u8]) -> bool {
    let Some(rest) = p.strip_prefix(b"/") else {
        return false;
    };
    let mut segments = rest.split(|&c| c == b'/').peekable();
    while let Some(segment) = segments.next() {
        match segment {
            b"." | b".." => return false,
            b"" if segments.peek().is_some() => return false,
            _ => {}
        }
    }
    true
}

/// Go's `url.unescape` in path mode: decodes every `%XX`; a `%` without
/// two hex digits after it is an error. Borrows when there is no `%`.
pub fn unescape(s: &[u8]) -> Result<Cow<'_, [u8]>, TargetError> {
    let Some(first) = s.iter().position(|&c| c == b'%') else {
        return Ok(Cow::Borrowed(s));
    };
    let mut out = Vec::with_capacity(s.len());
    out.extend_from_slice(&s[..first]);
    let mut i = first;
    while i < s.len() {
        if s[i] != b'%' {
            out.push(s[i]);
            i += 1;
            continue;
        }
        match (s.get(i + 1).and_then(hex), s.get(i + 2).and_then(hex)) {
            (Some(hi), Some(lo)) => out.push(hi << 4 | lo),
            _ => {
                return Err(TargetError::InvalidEscape(
                    s[i..s.len().min(i + 3)].to_vec(),
                ));
            }
        }
        i += 3;
    }
    Ok(Cow::Owned(out))
}

fn hex(c: &u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Go's `url.shouldEscape` in path mode: everything but letters, digits,
/// `-_.~` and the reserved characters a path may hold as they are
/// (`$&+,/:;=@`) is escaped.
fn should_escape(c: u8) -> bool {
    !(c.is_ascii_alphanumeric()
        || matches!(
            c,
            b'-' | b'_'
                | b'.'
                | b'~'
                | b'$'
                | b'&'
                | b'+'
                | b','
                | b'/'
                | b':'
                | b';'
                | b'='
                | b'@'
        ))
}

/// Go's `url.escape` in path mode, with upper-case hex digits.
pub fn escape(s: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = Vec::with_capacity(s.len() + 8);
    for &c in s {
        if should_escape(c) {
            out.extend_from_slice(&[b'%', HEX[usize::from(c >> 4)], HEX[usize::from(c & 15)]]);
        } else {
            out.push(c);
        }
    }
    out
}

/// Go's `url.validEncoded` in path mode: whether `s` may be sent as it is.
/// Sub-delimiters, `[`, `]` and `%` are allowed on top of what
/// [`escape`] leaves alone.
fn valid_encoded(s: &[u8]) -> bool {
    s.iter().all(|&c| {
        matches!(
            c,
            b'!' | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
                | b'['
                | b']'
                | b'%'
        ) || !should_escape(c)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: &[u8]) -> &str {
        std::str::from_utf8(b).unwrap()
    }

    /// steer's `TestCleanPath`, case for case.
    #[test]
    fn steer_clean_path_vectors() {
        let cases = [
            ("", "/"),
            ("/", "/"),
            ("a/b", "/a/b"),
            ("/a/b/", "/a/b/"),
            ("//a///b//", "/a/b/"),
            ("/public/../admin", "/admin"),
            ("/../../etc/passwd", "/etc/passwd"),
            ("/a/./b/.", "/a/b"),
            ("/a/..", "/"),
            ("/a/../", "/"),
        ];
        for (input, want) in cases {
            assert_eq!(s(&clean_path(input.as_bytes())), want, "{input:?}");
        }
    }

    #[test]
    fn clean_paths_are_borrowed() {
        for p in ["/", "/a", "/a/", "/a/b.c/...", "/.a/..b/"] {
            assert!(matches!(clean_path(p.as_bytes()), Cow::Borrowed(_)), "{p}");
        }
        for p in ["", "a", "//", "/a//b", "/./", "/a/.", "/a/..", "/../"] {
            assert!(matches!(clean_path(p.as_bytes()), Cow::Owned(_)), "{p}");
        }
    }

    #[test]
    fn clean_path_edge_cases() {
        let cases = [
            ("/..", "/"),
            ("/.", "/"),
            ("/...", "/..."),
            ("/a/b/../../..", "/"),
            ("/a//../b", "/b"),
            ("/a/b/c/../../d/", "/a/d/"),
            ("..", "/"),
            ("./", "/"),
            ("a/../../b/", "/b/"),
        ];
        for (input, want) in cases {
            assert_eq!(s(&clean_path(input.as_bytes())), want, "{input:?}");
        }
        // Bytes that are not UTF-8 are path bytes like any other.
        assert_eq!(&*clean_path(b"/\xff/./x/../\xfe"), b"/\xff/\xfe");
    }

    #[test]
    fn unescape_follows_go() {
        assert_eq!(&*unescape(b"/a%2Fb%2f%41+").unwrap(), b"/a/b/A+");
        assert_eq!(&*unescape(b"%ff%00").unwrap(), b"\xff\x00");
        assert!(matches!(unescape(b"/plain").unwrap(), Cow::Borrowed(_)));
        for (bad, text) in [
            (&b"/%"[..], &b"%"[..]),
            (b"/%2", b"%2"),
            (b"/%zz/x", b"%zz"),
            (b"/%2g", b"%2g"),
            (b"/a%g2", b"%g2"),
        ] {
            assert_eq!(
                unescape(bad),
                Err(TargetError::InvalidEscape(text.to_vec())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn escape_follows_go() {
        assert_eq!(s(&escape(b"/a b?c#d")), "/a%20b%3Fc%23d");
        assert_eq!(s(&escape(b"/$&+,/:;=@-_.~")), "/$&+,/:;=@-_.~");
        assert_eq!(s(&escape(b"*!'()[]%\"")), "%2A%21%27%28%29%5B%5D%25%22");
        assert_eq!(s(&escape("/é\x00\x7f".as_bytes())), "/%C3%A9%00%7F");
        assert!(valid_encoded(b"/!$&'()*+,;=:@[]%41-_.~"));
        for bad in [&b"/a b"[..], b"/a?", b"/\"", b"/\xff", b"/{"] {
            assert!(!valid_encoded(bad), "{bad:?}");
        }
    }

    #[test]
    fn parse_rejects_what_go_rejects() {
        assert_eq!(RequestTarget::parse(b""), Err(TargetError::Empty));
        assert_eq!(
            RequestTarget::parse(b"/a\x01"),
            Err(TargetError::ControlByte)
        );
        assert_eq!(
            RequestTarget::parse(b"/a?\x7f"),
            Err(TargetError::ControlByte)
        );
        assert_eq!(RequestTarget::parse(b"\x00"), Err(TargetError::ControlByte));
        for t in [&b"a/b"[..], b"?a", b"*?x", b"**", b"%2Fa"] {
            assert_eq!(
                RequestTarget::parse(t),
                Err(TargetError::NotOriginForm),
                "{t:?}"
            );
        }
        assert_eq!(
            RequestTarget::parse(b"/%zz?q"),
            Err(TargetError::InvalidEscape(b"%zz".to_vec()))
        );
        // The query is not decoded, so a bad escape there is fine.
        assert!(RequestTarget::parse(b"/a?%zz").is_ok());
    }

    #[test]
    fn errors_display() {
        assert_eq!(TargetError::Empty.to_string(), "empty request target");
        assert_eq!(
            TargetError::ControlByte.to_string(),
            "invalid control character in request target"
        );
        assert_eq!(
            TargetError::NotOriginForm.to_string(),
            "invalid URI for request"
        );
        assert_eq!(
            TargetError::InvalidEscape(b"%zz".to_vec()).to_string(),
            r#"invalid URL escape "%zz""#
        );
    }

    #[test]
    fn steer_forwards_the_cleaned_path_and_the_query() {
        // From steer's TestForwardProxyHeaders.
        let t = RequestTarget::parse(b"/public/../admin/x?q=1").unwrap();
        assert_eq!(t.path(), b"/public/../admin/x");
        assert_eq!(t.clean_path(), b"/admin/x");
        assert!(t.was_cleaned());
        assert_eq!(t.query(), Some(&b"q=1"[..]));
        assert_eq!(&*t.upstream_target(), b"/admin/x?q=1");
    }

    #[test]
    fn encoded_separators() {
        // Decoded before cleaning, so an encoded traversal is resolved.
        let t = RequestTarget::parse(b"/public%2F..%2Fadmin").unwrap();
        assert_eq!(t.clean_path(), b"/admin");
        assert_eq!(&*t.upstream_target(), b"/admin");
        // Nothing to clean: routed on the decoded path, sent as received.
        let t = RequestTarget::parse(b"/a%2fb?x").unwrap();
        assert!(!t.was_cleaned());
        assert_eq!(t.clean_path(), b"/a/b");
        assert!(matches!(t.upstream_target(), Cow::Borrowed(b"/a%2fb?x")));
        // A double-encoded slash is one decode away from a slash only.
        let t = RequestTarget::parse(b"/a%252Fb").unwrap();
        assert_eq!(t.clean_path(), b"/a%2Fb");
        assert_eq!(&*t.upstream_target(), b"/a%252Fb");
    }

    #[test]
    fn upstream_target_reencodes_like_go() {
        let cases: [(&[u8], &[u8]); 9] = [
            (b"/a b", b"/a%20b"),
            (b"/a b?q", b"/a%20b?q"),
            (b"/caf\xc3\xa9", b"/caf%C3%A9"),
            (b"/x/../%ff y", b"/%FF%20y"),
            (b"/a?", b"/a?"),
            (b"/a//b?", b"/a/b?"),
            (b"/a??", b"/a??"),
            (b"*", b"/%2A"),
            (b"/\"{}", b"/%22%7B%7D"),
        ];
        for (target, want) in cases {
            let t = RequestTarget::parse(target).unwrap();
            assert_eq!(
                &*t.upstream_target(),
                want,
                "{:?}",
                String::from_utf8_lossy(target)
            );
        }
        let star = RequestTarget::parse(b"*").unwrap();
        assert_eq!(star.path(), b"*");
        assert_eq!(&*star.escaped_path(), b"*");
        assert_eq!(star.query(), None);
        let plain = RequestTarget::parse(b"/a/b").unwrap();
        assert!(matches!(plain.upstream_target(), Cow::Borrowed(b"/a/b")));
        assert!(matches!(plain.escaped_path(), Cow::Borrowed(b"/a/b")));
    }
}
