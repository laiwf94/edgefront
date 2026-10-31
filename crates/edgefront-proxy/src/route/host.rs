//! Host header normalisation, ported from steer's `route.NormalizeHost`.

use std::borrow::Cow;

/// Lower-cases a Host header value and strips any port and one trailing dot,
/// exactly as steer's `NormalizeHost` does.
///
/// The port is removed with the rules of Go's `net.SplitHostPort`; a value it
/// would reject keeps its text, except that an IPv6 literal in brackets
/// without a port (`"[::1]"`) loses the brackets. Lower-casing follows Go's
/// `strings.ToLower`: ASCII letters, and other characters by their simple
/// (one character) Unicode mapping.
///
/// Values that are already lower case, with no port, brackets or trailing
/// dot, are returned borrowed, so the common case does not allocate.
pub fn normalize_host(h: &str) -> Cow<'_, str> {
    let h = match split_host_port(h) {
        Some(host) => host,
        None if h.len() >= 2 && h.starts_with('[') && h.ends_with(']') => &h[1..h.len() - 1],
        None => h,
    };
    let lower = to_lower(h);
    match lower {
        Cow::Borrowed(s) => Cow::Borrowed(s.strip_suffix('.').unwrap_or(s)),
        Cow::Owned(mut s) => {
            if s.ends_with('.') {
                s.pop();
            }
            Cow::Owned(s)
        }
    }
}

/// The host part of `hp` if Go's `net.SplitHostPort` accepts it, otherwise
/// `None`. Go accepts `host:port` and `[host]:port` (the port may be empty
/// or any text without a colon), and rejects a missing port, more than one
/// colon outside brackets, and stray brackets.
fn split_host_port(hp: &str) -> Option<&str> {
    let b = hp.as_bytes();
    let colon = b.iter().rposition(|&c| c == b':')?;
    let (host, open_from, close_from) = if b[0] == b'[' {
        let end = b.iter().position(|&c| c == b']')?;
        // The closing bracket must be followed directly by the last colon.
        if end + 1 != colon {
            return None;
        }
        (&hp[1..end], 1, end + 1)
    } else {
        let host = &hp[..colon];
        if host.contains(':') {
            return None;
        }
        (host, 0, 0)
    };
    if b[open_from..].contains(&b'[') || b[close_from..].contains(&b']') {
        return None;
    }
    Some(host)
}

/// Go's `strings.ToLower`: borrowed when nothing changes.
fn to_lower(s: &str) -> Cow<'_, str> {
    if s.is_ascii() {
        if s.bytes().any(|c| c.is_ascii_uppercase()) {
            return Cow::Owned(s.to_ascii_lowercase());
        }
        return Cow::Borrowed(s);
    }
    let lowered: String = s.chars().map(simple_lower).collect();
    if lowered == s {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(lowered)
    }
}

/// Go's `unicode.ToLower`, the simple case mapping. Rust's
/// `char::to_lowercase` gives the full mapping, which differs only where
/// the full mapping has more than one character: U+0130 (capital I with dot
/// above), which Go maps to a plain "i".
fn simple_lower(c: char) -> char {
    if c == '\u{130}' {
        return 'i';
    }
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) => l,
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inputs and the output of steer's NormalizeHost for each, produced by
    /// running the Go function on this list.
    const GO_VECTORS: &[(&str, &str)] = &[
        ("", ""),
        ("example.com", "example.com"),
        ("Example.COM", "example.com"),
        ("example.com.", "example.com"),
        ("example.com..", "example.com."),
        ("SHOP.example.com:8443", "shop.example.com"),
        ("shop.example.com.:80", "shop.example.com"),
        ("example.com:", "example.com"),
        (":80", ""),
        (":", ""),
        ("[::1]:8080", "::1"),
        ("[::1]", "::1"),
        ("[::1]:", "::1"),
        ("[FE80::1%25ETH0]:443", "fe80::1%25eth0"),
        ("::1", "::1"),
        ("a:b:c", "a:b:c"),
        ("[::1]x:80", "[::1]x:80"),
        ("[::1]:80:90", "[::1]:80:90"),
        ("[a]:b]", "a]:b"),
        ("[a[b]:80", "[a[b]:80"),
        ("a[b:80", "a[b:80"),
        ("a]b:80", "a]b:80"),
        ("[]", ""),
        ("[]:1", ""),
        ("[", "["),
        ("]", "]"),
        ("[x", "[x"),
        ("x]", "x]"),
        ("[a.com.]", "a.com"),
        ("A.B.", "a.b"),
        ("Ünicode.Example", "ünicode.example"),
        ("\u{130}stanbul.example", "istanbul.example"),
        ("\u{212A}.example", "k.example"),
        ("ΣΑΣ.example", "σασ.example"),
        ("already.lower", "already.lower"),
        ("ünicode.lower", "ünicode.lower"),
    ];

    #[test]
    fn matches_go_normalize_host() {
        for &(input, want) in GO_VECTORS {
            assert_eq!(normalize_host(input), want, "normalize_host({input:?})");
        }
    }

    #[test]
    fn borrows_when_nothing_changes() {
        for input in ["example.com", "a.b.c", "ünicode.lower", "x"] {
            assert!(
                matches!(normalize_host(input), Cow::Borrowed(_)),
                "{input:?}"
            );
        }
        for input in ["example.com:80", "[::1]", "example.com."] {
            assert!(
                matches!(normalize_host(input), Cow::Borrowed(_)),
                "{input:?}"
            );
        }
        for input in ["Example.com", "EXAMPLE.COM.", "Ünicode"] {
            assert!(matches!(normalize_host(input), Cow::Owned(_)), "{input:?}");
        }
    }

    #[test]
    fn simple_lower_keeps_characters_without_a_single_lower_form() {
        assert_eq!(simple_lower('A'), 'a');
        assert_eq!(simple_lower('a'), 'a');
        assert_eq!(simple_lower('\u{130}'), 'i');
        assert_eq!(simple_lower('ß'), 'ß');
        assert_eq!(simple_lower('7'), '7');
    }
}
