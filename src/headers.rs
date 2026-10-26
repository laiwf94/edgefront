//! Header field helpers: HTTP dates, entity tags, field-name lists and
//! hop-by-hop removal (RFC 9110 sections 5.6, 7.6.1 and 8.8.3).

use std::time::SystemTime;

use http::header::{self, HeaderMap, HeaderName, HeaderValue};

use crate::cache_control::{CacheControl, parse_delta};

impl CacheControl {
    /// Parses the Cache-Control field lines of a header map.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        CacheControl::parse(
            headers
                .get_all(header::CACHE_CONTROL)
                .iter()
                .map(HeaderValue::as_bytes),
        )
    }
}

/// Parses an HTTP-date in any of the three formats a recipient must accept.
pub fn parse_http_date(value: &HeaderValue) -> Option<SystemTime> {
    httpdate::parse_http_date(value.to_str().ok()?.trim()).ok()
}

/// The first value of a date header, if it parses.
pub fn date_header(headers: &HeaderMap, name: &HeaderName) -> Option<SystemTime> {
    headers.get(name).and_then(parse_http_date)
}

/// Formats a time as an IMF-fixdate.
pub fn http_date(t: SystemTime) -> HeaderValue {
    HeaderValue::from_str(&httpdate::fmt_http_date(t)).expect("IMF-fixdate is a valid header value")
}

/// The Age header in seconds; missing or invalid values count as zero.
pub fn age_value(headers: &HeaderMap) -> u64 {
    headers
        .get(header::AGE)
        .and_then(|v| parse_delta(v.as_bytes().trim_ascii()))
        .unwrap_or(0)
}

/// An entity tag. `opaque` excludes the surrounding quotes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityTag {
    pub weak: bool,
    pub opaque: String,
}

impl EntityTag {
    /// Parses a single entity tag such as `"abc"` or `W/"abc"`.
    pub fn parse(value: &[u8]) -> Option<Self> {
        let (tag, rest) = take_etag(value.trim_ascii())?;
        rest.is_empty().then_some(tag)
    }

    pub fn from_header(headers: &HeaderMap) -> Option<Self> {
        headers
            .get(header::ETAG)
            .and_then(|v| Self::parse(v.as_bytes()))
    }

    /// Strong comparison: both tags strong and identical.
    pub fn strong_eq(&self, other: &EntityTag) -> bool {
        !self.weak && !other.weak && self.opaque == other.opaque
    }

    /// Weak comparison: identical opaque tags, weakness ignored.
    pub fn weak_eq(&self, other: &EntityTag) -> bool {
        self.opaque == other.opaque
    }

    pub fn to_header_value(&self) -> HeaderValue {
        let s = if self.weak {
            format!("W/\"{}\"", self.opaque)
        } else {
            format!("\"{}\"", self.opaque)
        };
        HeaderValue::from_str(&s).expect("entity tag characters are visible ASCII")
    }
}

/// Reads one entity tag from the front of `input` and returns the rest.
fn take_etag(input: &[u8]) -> Option<(EntityTag, &[u8])> {
    let (weak, rest) = match input.strip_prefix(b"W/") {
        Some(rest) => (true, rest),
        None => (false, input),
    };
    let rest = rest.strip_prefix(b"\"")?;
    let end = rest.iter().position(|&b| b == b'"')?;
    let opaque = &rest[..end];
    // etagc is %x21 / %x23-7E; non-ASCII obs-text is refused.
    if !opaque
        .iter()
        .all(|&b| b == 0x21 || (0x23..=0x7e).contains(&b))
    {
        return None;
    }
    let opaque = String::from_utf8(opaque.to_vec()).ok()?;
    Some((EntityTag { weak, opaque }, &rest[end + 1..]))
}

/// The value of an `If-None-Match` or `If-Match` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EtagMatch {
    Any,
    Tags(Vec<EntityTag>),
}

impl EtagMatch {
    /// Parses every field line of `name`. Returns `None` when the header is
    /// absent or malformed, in which case the condition is ignored.
    pub fn from_headers(headers: &HeaderMap, name: &HeaderName) -> Option<Self> {
        let mut tags = Vec::new();
        let mut seen = false;
        for line in headers.get_all(name) {
            seen = true;
            let mut rest = line.as_bytes().trim_ascii();
            if rest == b"*" {
                return Some(EtagMatch::Any);
            }
            loop {
                while let [b',' | b' ' | b'\t', tail @ ..] = rest {
                    rest = tail;
                }
                if rest.is_empty() {
                    break;
                }
                let (tag, tail) = take_etag(rest)?;
                tags.push(tag);
                rest = tail;
                if let [first, ..] = rest
                    && !matches!(first, b',' | b' ' | b'\t')
                {
                    return None;
                }
            }
        }
        (seen && !tags.is_empty()).then_some(EtagMatch::Tags(tags))
    }

    /// True when any listed tag weakly matches `etag` (the If-None-Match rule).
    pub fn weak_matches(&self, etag: Option<&EntityTag>) -> bool {
        match (self, etag) {
            (EtagMatch::Any, Some(_)) => true,
            (EtagMatch::Tags(tags), Some(e)) => tags.iter().any(|t| t.weak_eq(e)),
            (_, None) => false,
        }
    }
}

/// Splits comma-separated field lines (Vary, Connection) into lower-case
/// members, dropping empty ones.
pub fn field_list(headers: &HeaderMap, name: &HeaderName) -> Vec<String> {
    let mut out = Vec::new();
    for line in headers.get_all(name) {
        let Ok(s) = line.to_str() else { continue };
        for member in s.split(',') {
            let member = member.trim();
            if !member.is_empty() {
                out.push(member.to_ascii_lowercase());
            }
        }
    }
    out
}

/// Header fields that apply to a single connection (RFC 9110 section 7.6.1).
const HOP_BY_HOP: [HeaderName; 9] = [
    header::CONNECTION,
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-connection"),
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// Removes hop-by-hop fields, including any named in Connection.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in field_list(headers, &header::CONNECTION) {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
    for name in &HOP_BY_HOP {
        headers.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn tag(s: &str) -> EntityTag {
        EntityTag::parse(s.as_bytes()).unwrap()
    }

    #[test]
    fn cache_control_from_several_lines() {
        let h = map(&[("cache-control", "max-age=5"), ("cache-control", "public")]);
        let cc = CacheControl::from_headers(&h);
        assert_eq!(cc.max_age, Some(5));
        assert!(cc.public);
    }

    #[test]
    fn dates_in_all_three_formats() {
        let want = UNIX_EPOCH + Duration::from_secs(784_111_777);
        for s in [
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
        ] {
            assert_eq!(
                parse_http_date(&HeaderValue::from_static(s)),
                Some(want),
                "{s}"
            );
        }
        assert_eq!(parse_http_date(&HeaderValue::from_static("0")), None);
        assert_eq!(
            parse_http_date(&HeaderValue::from_bytes(b"\xff").unwrap()),
            None
        );
        assert_eq!(http_date(want), "Sun, 06 Nov 1994 08:49:37 GMT");
        let h = map(&[("date", "Sun, 06 Nov 1994 08:49:37 GMT")]);
        assert_eq!(date_header(&h, &header::DATE), Some(want));
        assert_eq!(date_header(&h, &header::EXPIRES), None);
    }

    #[test]
    fn age_header() {
        assert_eq!(age_value(&map(&[("age", " 30 ")])), 30);
        assert_eq!(age_value(&map(&[("age", "-1")])), 0);
        assert_eq!(age_value(&HeaderMap::new()), 0);
    }

    #[test]
    fn entity_tag_parsing() {
        assert_eq!(
            tag("\"xyzzy\""),
            EntityTag {
                weak: false,
                opaque: "xyzzy".into()
            }
        );
        assert_eq!(
            tag("W/\"xyzzy\""),
            EntityTag {
                weak: true,
                opaque: "xyzzy".into()
            }
        );
        assert_eq!(
            tag("\"\""),
            EntityTag {
                weak: false,
                opaque: String::new()
            }
        );
        assert!(EntityTag::parse(b"xyzzy").is_none());
        assert!(EntityTag::parse(b"\"open").is_none());
        assert!(EntityTag::parse(b"\"a\" junk").is_none());
        assert!(EntityTag::parse(b"\"caf\xc3\xa9\"").is_none());
        assert!(EntityTag::parse(b"w/\"a\"").is_none());
    }

    #[test]
    fn entity_tag_round_trip() {
        for s in ["\"a\"", "W/\"a,b\""] {
            assert_eq!(tag(s).to_header_value(), s);
        }
        let h = map(&[("etag", "\"v1\"")]);
        assert_eq!(EntityTag::from_header(&h), Some(tag("\"v1\"")));
    }

    #[test]
    fn comparison_table_from_rfc_9110() {
        let cases = [
            ("W/\"1\"", "W/\"1\"", false, true),
            ("W/\"1\"", "W/\"2\"", false, false),
            ("W/\"1\"", "\"1\"", false, true),
            ("\"1\"", "\"1\"", true, true),
        ];
        for (a, b, strong, weak) in cases {
            assert_eq!(tag(a).strong_eq(&tag(b)), strong, "{a} {b}");
            assert_eq!(tag(a).weak_eq(&tag(b)), weak, "{a} {b}");
        }
    }

    #[test]
    fn if_none_match_lists() {
        let h = map(&[
            ("if-none-match", "\"a\", W/\"b\""),
            ("if-none-match", "\"c,d\""),
        ]);
        let m = EtagMatch::from_headers(&h, &header::IF_NONE_MATCH).unwrap();
        assert_eq!(
            m,
            EtagMatch::Tags(vec![tag("\"a\""), tag("W/\"b\""), tag("\"c,d\"")])
        );
        assert!(m.weak_matches(Some(&tag("\"b\""))));
        assert!(m.weak_matches(Some(&tag("\"c,d\""))));
        assert!(!m.weak_matches(Some(&tag("\"z\""))));
        assert!(!m.weak_matches(None));
    }

    #[test]
    fn if_none_match_star() {
        let h = map(&[("if-none-match", " * ")]);
        let m = EtagMatch::from_headers(&h, &header::IF_NONE_MATCH).unwrap();
        assert_eq!(m, EtagMatch::Any);
        assert!(m.weak_matches(Some(&tag("\"q\""))));
        assert!(!m.weak_matches(None));
    }

    #[test]
    fn if_none_match_malformed_or_absent() {
        assert_eq!(
            EtagMatch::from_headers(&HeaderMap::new(), &header::IF_NONE_MATCH),
            None
        );
        for bad in ["abc", "\"a\"\"b\"", "\"a\" x", ",,", "\"unterminated"] {
            let h = map(&[("if-none-match", bad)]);
            assert_eq!(
                EtagMatch::from_headers(&h, &header::IF_NONE_MATCH),
                None,
                "{bad}"
            );
        }
    }

    #[test]
    fn field_lists() {
        let h = map(&[
            ("vary", "Accept-Encoding, ,Accept-Language"),
            ("vary", "Origin"),
        ]);
        assert_eq!(
            field_list(&h, &header::VARY),
            ["accept-encoding", "accept-language", "origin"]
        );
        let mut h = HeaderMap::new();
        h.append(header::VARY, HeaderValue::from_bytes(b"\xff").unwrap());
        assert!(field_list(&h, &header::VARY).is_empty());
    }

    #[test]
    fn hop_by_hop_removal() {
        let mut h = map(&[
            ("connection", "close, X-Private, bad name"),
            ("x-private", "1"),
            ("keep-alive", "timeout=5"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "h2c"),
            ("content-type", "text/plain"),
        ]);
        strip_hop_by_hop(&mut h);
        let names: Vec<_> = h.keys().map(HeaderName::as_str).collect();
        assert_eq!(names, ["content-type"]);
    }
}
