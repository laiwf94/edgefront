//! Header field helpers: HTTP dates, entity tags, field-name lists and
//! hop-by-hop removal (RFC 9110 sections 5.6, 7.6.1 and 8.8.3).

use std::time::SystemTime;

use http::header::{self, HeaderMap, HeaderName, HeaderValue};

use crate::cache_control::parse_delta;

/// Parses an HTTP-date in any of the three formats a recipient must accept
/// (RFC 9110 section 5.6.7). Day names, month names and `GMT` are matched
/// in any case. An rfc850-date's two-digit year is read as the nearest
/// year with those digits that is at most 50 years in the future, as the
/// RFC requires, against the system clock.
pub fn parse_http_date(value: &HeaderValue) -> Option<SystemTime> {
    parse_http_date_at(value.to_str().ok()?, SystemTime::now())
}

const DAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// [`parse_http_date`] with `now` for the rfc850-date year.
pub(crate) fn parse_http_date_at(text: &str, now: SystemTime) -> Option<SystemTime> {
    let text = text.trim();
    if !text.is_ascii() {
        return None;
    }
    // Every alphabetic run in canonical case: day names (full or short),
    // month names and GMT.
    let mut canon = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_alphabetic() {
            canon.push(char::from(bytes[i]));
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let word = &text[start..i];
        let known = DAYS
            .iter()
            .flat_map(|d| [*d, &d[..3]])
            .chain(MONTHS)
            .chain(["GMT"])
            .find(|k| k.eq_ignore_ascii_case(word));
        canon.push_str(known.unwrap_or(word));
    }
    if let Some(imf) = rfc850_as_imf(&canon, now) {
        return httpdate::parse_http_date(&imf).ok();
    }
    httpdate::parse_http_date(&canon).ok()
}

/// An rfc850-date (`Sunday, 06-Nov-94 08:49:37 GMT`) rewritten as an
/// IMF-fixdate with its year resolved against `now`.
fn rfc850_as_imf(s: &str, now: SystemTime) -> Option<String> {
    let (day, rest) = s.split_once(", ")?;
    let day = DAYS.iter().find(|d| **d == day)?;
    let b = rest.as_bytes();
    if b.len() != 22 || b[2] != b'-' || b[6] != b'-' || &rest[18..] != " GMT" {
        return None;
    }
    let yy: u64 = rest.get(7..9)?.parse().ok()?;
    let this_year = httpdate::fmt_http_date(now)
        .get(12..16)?
        .parse::<u64>()
        .ok()?;
    let mut year = this_year - this_year % 100 + yy;
    if year > this_year + 50 {
        year -= 100;
    } else if year + 50 < this_year {
        year += 100;
    }
    Some(format!(
        "{}, {} {} {}{}",
        &day[..3],
        &rest[..2],
        &rest[3..6],
        year,
        &rest[9..]
    ))
}

/// The first value of a date header, if it parses.
pub(crate) fn date_header(headers: &HeaderMap, name: &HeaderName) -> Option<SystemTime> {
    headers.get(name).and_then(parse_http_date)
}

/// Formats a time as an IMF-fixdate.
pub fn http_date(t: SystemTime) -> HeaderValue {
    HeaderValue::from_str(&httpdate::fmt_http_date(t)).expect("IMF-fixdate is a valid header value")
}

/// The Age header in seconds; missing or invalid values count as zero.
/// A field line holding a list (`7200, 0`) is read by its first member,
/// as a list-based recipient would; later field lines are ignored.
pub(crate) fn age_value(headers: &HeaderMap) -> u64 {
    headers
        .get(header::AGE)
        .and_then(|v| {
            let first = v.as_bytes().split(|&b| b == b',').next()?;
            parse_delta(first.trim_ascii())
        })
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
pub(crate) fn field_list(headers: &HeaderMap, name: &HeaderName) -> Vec<String> {
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

    #[test]
    fn dates_in_any_case_and_rfc850_years_within_fifty_years() {
        let at = |s: &str| parse_http_date_at(s, UNIX_EPOCH + Duration::from_secs(1_795_000_000));
        let want = httpdate::parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        for s in [
            "SUN, 06 NOV 1994 08:49:37 gmt",
            "sun, 06 nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "sunday, 06-nov-94 08:49:37 Gmt",
            "Sun Nov  6 08:49:37 1994",
        ] {
            assert_eq!(at(s), Some(want), "{s}");
        }
        // 2026: 70 is 2070 (44 years ahead), 74 is 2074, 86 is 1986.
        let year = |s: &str| at(s).map(|t| httpdate::fmt_http_date(t)[12..16].to_owned());
        assert_eq!(
            year("Saturday, 01-Feb-70 00:00:00 GMT").as_deref(),
            Some("2070")
        );
        assert_eq!(
            year("Thursday, 01-Feb-74 00:00:00 GMT").as_deref(),
            Some("2074")
        );
        assert_eq!(
            year("Saturday, 01-Feb-86 00:00:00 GMT").as_deref(),
            Some("1986")
        );
        // The weekday must match the resolved date.
        assert_eq!(at("Monday, 01-Feb-70 00:00:00 GMT"), None);
        for bad in [
            "Sunday, 06-Nov-94 08:49:37 UTC",
            "Sunday 06-Nov-94 08:49:37 GMT",
            "Someday, 06-Nov-94 08:49:37 GMT",
            "Sunday, 06-Nov-9x 08:49:37 GMT",
            "Sunday, 06 Nov 94 08:49:37 GMT",
            "Sön, 06 Nov 1994 08:49:37 GMT",
            "Xyz, 06 Nov 1994 08:49:37 GMT",
        ] {
            assert_eq!(at(bad), None, "{bad}");
        }
        // Near a century's end the window reaches into the next one.
        let t2090 = httpdate::parse_http_date("Sun, 01 Jan 2090 00:00:00 GMT").unwrap();
        let y = parse_http_date_at("Wednesday, 01-Jan-10 00:00:00 GMT", t2090)
            .map(|t| httpdate::fmt_http_date(t)[12..16].to_owned());
        assert_eq!(y.as_deref(), Some("2110"));
    }

    #[test]
    fn age_lists_are_read_by_their_first_member() {
        assert_eq!(age_value(&map(&[("age", "7200, 0")])), 7200);
        assert_eq!(age_value(&map(&[("age", "0, 7200")])), 0);
        assert_eq!(age_value(&map(&[("age", "x, 5")])), 0);
        assert_eq!(age_value(&map(&[("age", "5"), ("age", "9")])), 5);
    }
}
