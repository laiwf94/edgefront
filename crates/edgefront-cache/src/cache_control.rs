//! Cache-Control parsing for requests and responses.
//!
//! Directives come from RFC 9111 section 5.2 and the two extensions in
//! RFC 5861. The parser accepts what real origins send: quoted and unquoted
//! arguments, stray whitespace, empty list members and unknown directives.
//! When a directive appears twice, the first occurrence wins (RFC 9111
//! section 4.2.1 allows that or treating the response as stale).
//!
//! A response's directives for this cache come from `CDN-Cache-Control`
//! (RFC 9213) when it is present and valid, and from `Cache-Control`
//! otherwise ([`CacheControl::for_response`]). The targeted field is a
//! Structured Fields dictionary (RFC 9651); one that does not parse is
//! ignored as a whole, and a directive whose value has the wrong type is
//! ignored on its own.

use http::header::{self, HeaderMap, HeaderValue};

/// The value used when delta-seconds overflows (RFC 9111 section 1.2.2):
/// at least 2^31, or the greatest integer the cache can conveniently
/// represent. 2^32 - 1, so an Age just under 2^31 with a `max-age` of
/// 2^32 - 1 is still fresh, and lifetimes stay well inside `SystemTime`.
pub const DELTA_SECONDS_MAX: u64 = u32::MAX as u64;

/// The targeted cache-control field for CDN caches (RFC 9213).
pub const CDN_CACHE_CONTROL: header::HeaderName =
    header::HeaderName::from_static("cdn-cache-control");

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
    /// Set when these directives came from `CDN-Cache-Control`, which also
    /// makes this cache ignore `Expires` (RFC 9213 section 2.2).
    pub targeted: bool,
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

/// Parses delta-seconds: one or more ASCII digits, clamped to
/// [`DELTA_SECONDS_MAX`].
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
    /// The directives a response gives this cache: `CDN-Cache-Control` when
    /// it is present and parses as a dictionary, otherwise `Cache-Control`.
    pub fn for_response(headers: &HeaderMap) -> Self {
        Self::targeted(headers).unwrap_or_else(|| Self::from_headers(headers))
    }

    /// The directives in `CDN-Cache-Control`, if present and valid.
    pub fn targeted(headers: &HeaderMap) -> Option<Self> {
        let mut lines = headers.get_all(CDN_CACHE_CONTROL).iter().peekable();
        lines.peek()?;
        let mut joined = Vec::new();
        for (i, line) in lines.enumerate() {
            if i > 0 {
                joined.extend_from_slice(b", ");
            }
            joined.extend_from_slice(line.as_bytes());
        }
        let members = sf_dictionary(&joined)?;
        let mut cc = CacheControl {
            targeted: true,
            ..CacheControl::default()
        };
        for (name, value) in &members {
            match (name.as_slice(), value) {
                (
                    b"max-age" | b"s-maxage" | b"stale-while-revalidate" | b"stale-if-error",
                    SfValue::Integer(n),
                ) => {
                    if let Ok(n) = u64::try_from(*n) {
                        cc.apply(name, Some(n.to_string().as_bytes()));
                    }
                }
                (b"no-cache" | b"private", SfValue::True | SfValue::String) => cc.apply(name, None),
                (name, SfValue::True)
                    if !matches!(
                        name,
                        b"max-age" | b"s-maxage" | b"stale-while-revalidate" | b"stale-if-error"
                    ) =>
                {
                    cc.apply(name, None);
                }
                _ => {}
            }
        }
        Some(cc)
    }

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

/// A dictionary member's value, reduced to what directives need.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SfValue {
    Integer(i64),
    True,
    String,
    /// Any other item or an inner list: a decimal, token, byte sequence,
    /// `?0`, a date or a display string.
    Other,
}

/// Parses a Structured Fields dictionary (RFC 9651 section 4.2.2) into its
/// members in order, a repeated key keeping the last value. `None` when the
/// field is not a valid dictionary.
fn sf_dictionary(input: &[u8]) -> Option<Vec<(Vec<u8>, SfValue)>> {
    let mut p = SfParser { s: input, i: 0 };
    p.skip_sp();
    let mut members: Vec<(Vec<u8>, SfValue)> = Vec::new();
    while !p.done() {
        let key = p.key()?;
        let value = if p.eat(b'=') {
            if p.peek() == Some(b'(') {
                p.inner_list()?;
                SfValue::Other
            } else {
                p.bare_item()?
            }
        } else {
            SfValue::True
        };
        p.params()?;
        match members.iter_mut().find(|(k, _)| *k == key) {
            Some(m) => m.1 = value,
            None => members.push((key, value)),
        }
        p.skip_ows();
        if p.done() {
            break;
        }
        if !p.eat(b',') {
            return None;
        }
        p.skip_ows();
        if p.done() {
            return None;
        }
    }
    p.skip_sp();
    p.done().then_some(members)
}

struct SfParser<'a> {
    s: &'a [u8],
    i: usize,
}

impl SfParser<'_> {
    fn done(&self) -> bool {
        self.i >= self.s.len()
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, b: u8) -> bool {
        let hit = self.peek() == Some(b);
        if hit {
            self.i += 1;
        }
        hit
    }

    fn skip_sp(&mut self) {
        while self.eat(b' ') {}
    }

    fn skip_ows(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.i += 1;
        }
    }

    fn key(&mut self) -> Option<Vec<u8>> {
        let start = self.i;
        match self.peek() {
            Some(b'a'..=b'z' | b'*') => self.i += 1,
            _ => return None,
        }
        while matches!(
            self.peek(),
            Some(b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'*')
        ) {
            self.i += 1;
        }
        Some(self.s[start..self.i].to_vec())
    }

    fn params(&mut self) -> Option<()> {
        while self.eat(b';') {
            self.skip_sp();
            self.key()?;
            if self.eat(b'=') {
                self.bare_item()?;
            }
        }
        Some(())
    }

    fn inner_list(&mut self) -> Option<()> {
        if !self.eat(b'(') {
            return None;
        }
        loop {
            self.skip_sp();
            if self.eat(b')') {
                return self.params();
            }
            self.bare_item()?;
            self.params()?;
            if !matches!(self.peek(), Some(b' ' | b')')) {
                return None;
            }
        }
    }

    fn bare_item(&mut self) -> Option<SfValue> {
        match self.peek()? {
            b'-' | b'0'..=b'9' => self.number(),
            b'"' => self.string(),
            b'?' => {
                self.i += 1;
                match self.peek() {
                    Some(b'1') => {
                        self.i += 1;
                        Some(SfValue::True)
                    }
                    Some(b'0') => {
                        self.i += 1;
                        Some(SfValue::Other)
                    }
                    _ => None,
                }
            }
            b':' => {
                self.i += 1;
                while matches!(self.peek(), Some(b) if b.is_ascii_alphanumeric() || b"+/=".contains(&b))
                {
                    self.i += 1;
                }
                self.eat(b':').then_some(SfValue::Other)
            }
            b'@' => {
                self.i += 1;
                match self.number()? {
                    SfValue::Integer(_) => Some(SfValue::Other),
                    _ => None,
                }
            }
            b'%' => {
                self.i += 1;
                self.string().map(|_| SfValue::Other)
            }
            b if b.is_ascii_alphabetic() || b == b'*' => {
                self.i += 1;
                while matches!(self.peek(), Some(b) if is_tchar(b) || b == b':' || b == b'/') {
                    self.i += 1;
                }
                Some(SfValue::Other)
            }
            _ => None,
        }
    }

    fn number(&mut self) -> Option<SfValue> {
        let negative = self.eat(b'-');
        let start = self.i;
        let mut dot = None;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                self.i += 1;
            } else if b == b'.' && dot.is_none() && self.i > start {
                dot = Some(self.i);
                self.i += 1;
            } else {
                break;
            }
            if dot.is_none() && self.i - start > 15 {
                return None;
            }
        }
        let digits = &self.s[start..self.i];
        if digits.is_empty() || !digits[0].is_ascii_digit() {
            return None;
        }
        match dot {
            None => {
                let n: i64 = std::str::from_utf8(digits).ok()?.parse().ok()?;
                Some(SfValue::Integer(if negative { -n } else { n }))
            }
            Some(d) => {
                let (int, frac) = (d - start, self.i - d - 1);
                (int <= 12 && (1..=3).contains(&frac)).then_some(SfValue::Other)
            }
        }
    }

    fn string(&mut self) -> Option<SfValue> {
        if !self.eat(b'"') {
            return None;
        }
        loop {
            match self.peek()? {
                b'"' => {
                    self.i += 1;
                    return Some(SfValue::String);
                }
                b'\\' => {
                    self.i += 1;
                    match self.peek()? {
                        b'"' | b'\\' => self.i += 1,
                        _ => return None,
                    }
                }
                0x20..=0x7e => self.i += 1,
                _ => return None,
            }
        }
    }
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
    fn overflow_clamps_to_two_to_the_32_less_one() {
        assert_eq!(
            cc("max-age=99999999999999999999999").max_age,
            Some(DELTA_SECONDS_MAX)
        );
        assert_eq!(cc("max-age=4294967296").max_age, Some(DELTA_SECONDS_MAX));
        assert_eq!(cc("max-age=4294967295").max_age, Some(4_294_967_295));
        assert_eq!(cc("max-age=2147483648").max_age, Some(2_147_483_648));
        const { assert!(DELTA_SECONDS_MAX >= 1 << 31) };
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

    fn targeted(values: &[&str]) -> Option<CacheControl> {
        let mut h = HeaderMap::new();
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        for v in values {
            h.append(CDN_CACHE_CONTROL, HeaderValue::from_str(v).unwrap());
        }
        CacheControl::targeted(&h)
    }

    #[test]
    fn cdn_cache_control_replaces_cache_control_when_valid() {
        let c = targeted(&["max-age=3600"]).unwrap();
        assert!(c.targeted && !c.no_store);
        assert_eq!(c.max_age, Some(3600));
        // Several lines are one dictionary; a repeated key keeps the last.
        let c = targeted(&["max-age=1, private", "max-age=7;x=1, s-maxage=9"]).unwrap();
        assert_eq!((c.max_age, c.s_maxage, c.private), (Some(7), Some(9), true));
        // Unknown members and parameters are allowed; flags are booleans.
        let c = targeted(&["foobar, no-store, must-revalidate=?1, public=?0"]).unwrap();
        assert!(c.no_store && c.must_revalidate && !c.public);
        // Qualified private and no-cache count as the unqualified forms.
        let c = targeted(&["private=\"set-cookie\", no-cache=\"x\""]).unwrap();
        assert!(c.private && c.no_cache);
        // A wrong-typed value drops only its directive.
        let c = targeted(&["max-age=\"10000\", s-maxage=1.5, stale-if-error, no-store=5"]).unwrap();
        assert_eq!(
            (c.max_age, c.s_maxage, c.stale_if_error),
            (None, None, None)
        );
        assert!(!c.no_store);
        assert_eq!(targeted(&["max-age=-1"]).unwrap().max_age, None);
        assert_eq!(
            targeted(&["max-age=99999999999"]).unwrap().max_age,
            Some(DELTA_SECONDS_MAX)
        );
        // Every item type parses.
        let c = targeted(&[
            "a=tok/en:x, b=:aGk=:, c=@1700000000, d=%\"x\", e=(1 \"two\" ?1);p, f=12.345, g=\"q\\\"\", max-age=5",
        ])
        .unwrap();
        assert_eq!(c.max_age, Some(5));
        // Absent, or not a dictionary: none, and Cache-Control applies.
        assert_eq!(targeted(&[]), None);
        for bad in [
            "max-age=10000, &&&&&",
            "MaX-aGe=3600",
            "max-age=1,",
            "max-age =1",
            "max-age=1 2",
            "a=\"unterminated",
            "a=\"bad\\escape\"",
            "a=1.2345",
            "a=1234567890123.4",
            "a=1234567890123456",
            "a=?2",
            "a=(1 2",
            "a=(1;)",
            "a=:bad",
            "a=@x",
            "a=-",
            "a=",
            "a;=1",
            ", a",
        ] {
            assert_eq!(targeted(&[bad]), None, "{bad}");
        }
        let mut h = HeaderMap::new();
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        h.insert(CDN_CACHE_CONTROL, HeaderValue::from_static("max-age=1, &"));
        let c = CacheControl::for_response(&h);
        assert!(c.no_store && !c.targeted);
        h.insert(CDN_CACHE_CONTROL, HeaderValue::from_static("max-age=1"));
        assert!(!CacheControl::for_response(&h).no_store);
    }
}
