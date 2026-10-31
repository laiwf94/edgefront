//! Cache-Control parsing for requests and responses.
//!
//! Directives come from RFC 9111 section 5.2 and the two extensions in
//! RFC 5861. The parser accepts what real origins send: quoted and unquoted
//! arguments, stray whitespace, empty list members and unknown directives.
//! When a directive appears twice, the first occurrence wins (RFC 9111
//! section 4.2.1 allows that or treating the response as stale).

use http::header::{self, HeaderMap, HeaderValue};

/// The value used when delta-seconds overflows (RFC 9111 section 1.2.2).
pub const DELTA_SECONDS_MAX: u64 = 1 << 31;

/// Parsed Cache-Control directives. Request-only and response-only
/// directives share one type because their names do not collide.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheControl {
    pub max_age: Option<u64>,
    pub s_maxage: Option<u64>,
    /// `max-stale` without an argument means any staleness, kept as `u64::MAX`.
    pub max_stale: Option<u64>,
    pub min_fresh: Option<u64>,
    pub stale_while_revalidate: Option<u64>,
    pub stale_if_error: Option<u64>,
    pub no_cache: bool,
    pub no_store: bool,
    pub no_transform: bool,
    pub only_if_cached: bool,
    pub must_revalidate: bool,
    pub proxy_revalidate: bool,
    pub must_understand: bool,
    pub private: bool,
    pub public: bool,
    pub immutable: bool,
}

impl CacheControl {
    /// Parses every field line of a Cache-Control header, in order.
    pub fn parse<'a>(lines: impl IntoIterator<Item = &'a [u8]>) -> Self {
        let mut cc = CacheControl::default();
        for line in lines {
            for_each_directive(line, |name, arg| cc.apply(name, arg));
        }
        cc
    }

    /// Parses a single field value.
    pub fn parse_str(value: &str) -> Self {
        Self::parse([value.as_bytes()])
    }

    fn apply(&mut self, name: &[u8], arg: Option<&[u8]>) {
        // Freshness directives on responses fail safe: an invalid argument
        // makes the response stale rather than being ignored.
        let lifetime = || arg.map_or(Some(0), |a| Some(parse_delta(a).unwrap_or(0)));
        let optional = || arg.and_then(parse_delta);
        // Lower-case into a stack buffer; no known directive is longer.
        let mut buf = [0u8; 24];
        let Some(lower) = buf.get_mut(..name.len()) else {
            return;
        };
        lower.copy_from_slice(name);
        lower.make_ascii_lowercase();
        match &*lower {
            b"max-age" => set_once(&mut self.max_age, lifetime()),
            b"s-maxage" => set_once(&mut self.s_maxage, lifetime()),
            b"max-stale" => match arg {
                None => set_once(&mut self.max_stale, Some(u64::MAX)),
                Some(a) => set_once(&mut self.max_stale, parse_delta(a)),
            },
            b"min-fresh" => set_once(&mut self.min_fresh, optional()),
            b"stale-while-revalidate" => set_once(&mut self.stale_while_revalidate, optional()),
            b"stale-if-error" => set_once(&mut self.stale_if_error, optional()),
            // Qualified no-cache and private (with field names) are treated
            // as the unqualified forms, which is the conservative reading.
            b"no-cache" => self.no_cache = true,
            b"no-store" => self.no_store = true,
            b"no-transform" => self.no_transform = true,
            b"only-if-cached" => self.only_if_cached = true,
            b"must-revalidate" => self.must_revalidate = true,
            b"proxy-revalidate" => self.proxy_revalidate = true,
            b"must-understand" => self.must_understand = true,
            b"private" => self.private = true,
            b"public" => self.public = true,
            b"immutable" => self.immutable = true,
            _ => {}
        }
    }
}

fn set_once(slot: &mut Option<u64>, value: Option<u64>) {
    if slot.is_none() {
        *slot = value;
    }
}

/// Parses delta-seconds: one or more ASCII digits, clamped to 2^31.
pub fn parse_delta(arg: &[u8]) -> Option<u64> {
    if arg.is_empty() || !arg.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut n: u64 = 0;
    for d in arg {
        n = n.saturating_mul(10).saturating_add(u64::from(d - b'0'));
        if n >= DELTA_SECONDS_MAX {
            return Some(DELTA_SECONDS_MAX);
        }
    }
    Some(n)
}

/// Calls `f(name, argument)` for each directive in a field value.
/// Quoted-string arguments are returned without quotes; escapes are kept
/// as-is because no directive we act on needs them.
fn for_each_directive(value: &[u8], mut f: impl FnMut(&[u8], Option<&[u8]>)) {
    let mut i = 0;
    let n = value.len();
    while i < n {
        while i < n && (value[i] == b',' || is_ows(value[i])) {
            i += 1;
        }
        let start = i;
        while i < n && is_tchar(value[i]) {
            i += 1;
        }
        let name = &value[start..i];
        while i < n && is_ows(value[i]) {
            i += 1;
        }
        let mut arg = None;
        if i < n && value[i] == b'=' {
            i += 1;
            while i < n && is_ows(value[i]) {
                i += 1;
            }
            if i < n && value[i] == b'"' {
                i += 1;
                let arg_start = i;
                while i < n && value[i] != b'"' {
                    if value[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                let arg_end = i.min(n);
                arg = Some(&value[arg_start..arg_end]);
                i = (i + 1).min(n);
            } else {
                let arg_start = i;
                while i < n && is_tchar(value[i]) {
                    i += 1;
                }
                arg = Some(&value[arg_start..i]);
            }
        }
        // Anything left before the next comma makes the member malformed.
        let clean = i >= n || value[i] == b',' || is_ows(value[i]);
        let mut trailing = false;
        while i < n && value[i] != b',' {
            if !is_ows(value[i]) {
                trailing = true;
            }
            i += 1;
        }
        if !name.is_empty() && clean && !trailing {
            f(name, arg);
        }
    }
}

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

fn is_ows(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

/// token characters from RFC 9110 section 5.6.2.
pub fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cc(s: &str) -> CacheControl {
        CacheControl::parse_str(s)
    }

    #[test]
    fn empty_value_has_no_directives() {
        assert_eq!(cc(""), CacheControl::default());
        assert_eq!(cc(" , ,, "), CacheControl::default());
    }

    #[test]
    fn boolean_directives() {
        let c = cc(
            "no-cache, no-store, no-transform, only-if-cached, must-revalidate, \
                    proxy-revalidate, must-understand, private, public, immutable",
        );
        assert!(c.no_cache && c.no_store && c.no_transform && c.only_if_cached);
        assert!(c.must_revalidate && c.proxy_revalidate && c.must_understand);
        assert!(c.private && c.public && c.immutable);
    }

    #[test]
    fn names_are_case_insensitive() {
        let c = cc("Max-Age=60, NO-STORE, S-MaxAge=5");
        assert_eq!(c.max_age, Some(60));
        assert_eq!(c.s_maxage, Some(5));
        assert!(c.no_store);
    }

    #[test]
    fn numeric_directives() {
        let c = cc("max-age=60, s-maxage=30, stale-while-revalidate=10, stale-if-error=600");
        assert_eq!(c.max_age, Some(60));
        assert_eq!(c.s_maxage, Some(30));
        assert_eq!(c.stale_while_revalidate, Some(10));
        assert_eq!(c.stale_if_error, Some(600));
        let r = cc("max-stale=5, min-fresh=7");
        assert_eq!(r.max_stale, Some(5));
        assert_eq!(r.min_fresh, Some(7));
    }

    #[test]
    fn max_stale_without_argument_allows_any_staleness() {
        assert_eq!(cc("max-stale").max_stale, Some(u64::MAX));
    }

    #[test]
    fn quoted_arguments_are_accepted() {
        assert_eq!(cc("max-age=\"120\"").max_age, Some(120));
        let c = cc("private=\"set-cookie, x-a\", max-age=3");
        assert!(c.private);
        assert_eq!(c.max_age, Some(3));
    }

    #[test]
    fn escaped_quote_inside_argument() {
        let c = cc(r#"ext="a\"b,c", max-age=4"#);
        assert_eq!(c.max_age, Some(4));
    }

    #[test]
    fn unterminated_quote_consumes_the_rest() {
        let c = cc("no-cache=\"a, max-age=4");
        assert!(c.no_cache);
        assert_eq!(c.max_age, None);
    }

    #[test]
    fn first_duplicate_wins() {
        assert_eq!(cc("max-age=10, max-age=20").max_age, Some(10));
        let c = CacheControl::parse([b"max-age=1".as_slice(), b"max-age=2".as_slice()]);
        assert_eq!(c.max_age, Some(1));
    }

    #[test]
    fn invalid_lifetime_means_stale() {
        assert_eq!(cc("max-age=abc").max_age, Some(0));
        assert_eq!(cc("max-age").max_age, Some(0));
        assert_eq!(cc("s-maxage=-1").s_maxage, Some(0));
        assert_eq!(cc("max-age=1.5").max_age, Some(0));
    }

    #[test]
    fn invalid_optional_arguments_are_ignored() {
        let c = cc("stale-while-revalidate=x, stale-if-error, min-fresh=, max-stale=q");
        assert_eq!(c.stale_while_revalidate, None);
        assert_eq!(c.stale_if_error, None);
        assert_eq!(c.min_fresh, None);
        assert_eq!(c.max_stale, None);
    }

    #[test]
    fn overflow_clamps_to_two_to_the_31() {
        assert_eq!(
            cc("max-age=99999999999999999999999").max_age,
            Some(DELTA_SECONDS_MAX)
        );
        assert_eq!(cc("max-age=2147483648").max_age, Some(DELTA_SECONDS_MAX));
        assert_eq!(cc("max-age=2147483647").max_age, Some(2_147_483_647));
    }

    #[test]
    fn whitespace_around_equals_is_tolerated() {
        assert_eq!(cc("max-age = 9").max_age, Some(9));
        assert_eq!(cc("\tmax-age=9\t,public").max_age, Some(9));
    }

    #[test]
    fn malformed_members_are_skipped() {
        let c = cc("max-age=5x;y, public, @@@, no-store");
        assert_eq!(c.max_age, None);
        assert!(c.public && c.no_store);
        let c = cc("max-age=5 junk, private");
        assert_eq!(c.max_age, None);
        assert!(c.private);
    }

    #[test]
    fn unknown_directives_are_ignored() {
        let c = cc("a-very-long-extension-directive-name=1, max-age=2");
        assert_eq!(c.max_age, Some(2));
        let c = cc("community=\"UCI\", foo, bar=1, max-age=1");
        assert_eq!(c.max_age, Some(1));
    }

    #[test]
    fn non_utf8_bytes_do_not_panic() {
        let c = CacheControl::parse([b"max-age=5, \xff\xfe, public".as_slice()]);
        assert_eq!(c.max_age, Some(5));
        assert!(c.public);
    }

    #[test]
    fn parse_delta_rules() {
        assert_eq!(parse_delta(b""), None);
        assert_eq!(parse_delta(b"0"), Some(0));
        assert_eq!(parse_delta(b"007"), Some(7));
        assert_eq!(parse_delta(b"+7"), None);
    }

    #[test]
    fn cache_control_from_several_lines() {
        let mut h = HeaderMap::new();
        h.append(header::CACHE_CONTROL, HeaderValue::from_static("max-age=5"));
        h.append(header::CACHE_CONTROL, HeaderValue::from_static("public"));
        let cc = CacheControl::from_headers(&h);
        assert_eq!(cc.max_age, Some(5));
        assert!(cc.public);
    }
}
