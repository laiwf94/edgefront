//! Request matchers beyond host and path prefix: the path as exact, prefix
//! or regex, the method, headers and query parameters.
//!
//! The semantics follow Envoy's route match, so configurations translate
//! one to one:
//!
//! - **Regexes match the whole value.** A pattern is compiled as
//!   `\A(?:pattern)\z`, as Envoy's `safe_regex` (RE2 full match) does, so
//!   `/api/v[0-9]+` does not match `/api/v1/users`. Patterns run on bytes,
//!   since a decoded path or a header value need not be UTF-8; `.` matches
//!   one UTF-8 encoded character and no invalid byte, and `(?-u:.)` matches
//!   any byte. Compiled programs are capped at [`REGEX_SIZE_LIMIT`] bytes,
//!   so a config cannot make the proxy build a huge automaton. Of Unicode's
//!   tables only the Perl classes (`\d`, `\w`, `\s`) and case folding are
//!   built into the edgefront binary, to keep it small: there `\p{Greek}`
//!   and other property classes are refused when the config is loaded.
//! - **Case.** Header names are case-insensitive (they are stored lower
//!   case). Query parameter names, methods (RFC 9110 section 9.1) and
//!   values are case-sensitive unless a value matcher sets `ignore_case`,
//!   which folds ASCII letters only, as Envoy does, and also makes a regex
//!   case-insensitive (where Envoy ignores it and asks for `(?i)`).
//! - **Several values.** A header sent more than once is matched as its
//!   values joined with `,`, as Envoy joins them. A query parameter given
//!   more than once is matched on its first value.
//! - **Presence and absence.** [`ValueTest::Present`] and
//!   [`ValueTest::Absent`] look only at whether the header or parameter is
//!   there; a parameter written without `=` (`?debug`) is present with an
//!   empty value. A value or range test on a missing header or parameter
//!   fails, even inverted, unless the matcher treats missing as empty: an
//!   inverted `exact: "1"` asks for a header that is there and is not `1`.
//!   Inverting `Present` or `Absent` simply swaps them.
//! - **Query strings** are split on `&`; each piece's name and value are
//!   split at the first `=` and percent-decoded. `+` is kept as is (it is
//!   form encoding, not URI encoding), and a `%` not followed by two hex
//!   digits is kept literally.
//!
//! A [`RequestMatch`] holds all of a route's conditions and matches when
//! every one does. It parses the query string at most once per request,
//! and only when a route asks about it.

use std::borrow::Cow;
use std::cell::OnceCell;
use std::fmt;
use std::ops::Range;

use http::Method;
use http::header::{HeaderMap, HeaderName};
use regex::bytes::{Regex, RegexBuilder};

/// The largest compiled regex program, in bytes, a matcher accepts. A
/// route pattern is short; this is far above any sensible one and far
/// below what would hurt a node.
pub const REGEX_SIZE_LIMIT: usize = 256 * 1024;

/// A matcher that could not be built from its config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchError(String);

impl fmt::Display for MatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MatchError {}

/// Compiles `pattern` to match whole values.
fn full_regex(pattern: &str, ignore_case: bool) -> Result<Regex, MatchError> {
    RegexBuilder::new(&format!(r"\A(?:{pattern})\z"))
        .case_insensitive(ignore_case)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| MatchError(format!("regex {pattern:?}: {e}")))
}

/// How a [`StringMatcher`] compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringKind {
    /// The whole value equals the pattern.
    Exact,
    /// The value starts with the pattern.
    Prefix,
    /// The value ends with the pattern.
    Suffix,
    /// The pattern occurs anywhere in the value.
    Contains,
    /// The whole value matches the pattern as a regex.
    Regex,
}

#[derive(Debug, Clone)]
enum Pattern {
    Exact(Box<[u8]>),
    Prefix(Box<[u8]>),
    Suffix(Box<[u8]>),
    /// Compiled to an escaped literal, so the search is the regex crate's
    /// substring search, ASCII case folding included.
    Contains(Regex),
    Regex(Regex),
}

/// A test on a string value: exact, prefix, suffix, contains or regex,
/// optionally ignoring ASCII case.
#[derive(Debug, Clone)]
pub struct StringMatcher {
    pattern: Pattern,
    ignore_case: bool,
}

impl StringMatcher {
    /// Builds a matcher. Fails only for a regex that does not compile or is
    /// over [`REGEX_SIZE_LIMIT`].
    pub fn new(kind: StringKind, pattern: &str, ignore_case: bool) -> Result<Self, MatchError> {
        let literal = || -> Box<[u8]> {
            if ignore_case {
                pattern.to_ascii_lowercase().into_bytes().into()
            } else {
                pattern.as_bytes().into()
            }
        };
        let pattern = match kind {
            StringKind::Exact => Pattern::Exact(literal()),
            StringKind::Prefix => Pattern::Prefix(literal()),
            StringKind::Suffix => Pattern::Suffix(literal()),
            StringKind::Contains => {
                let re = RegexBuilder::new(&regex::escape(pattern))
                    .unicode(false)
                    .case_insensitive(ignore_case)
                    .build()
                    .map_err(|e| MatchError(format!("contains {pattern:?}: {e}")))?;
                Pattern::Contains(re)
            }
            StringKind::Regex => Pattern::Regex(full_regex(pattern, ignore_case)?),
        };
        Ok(Self {
            pattern,
            ignore_case,
        })
    }

    /// An exact, case-sensitive match.
    pub fn exact(pattern: &str) -> Self {
        Self::new(StringKind::Exact, pattern, false).expect("literals always build")
    }

    /// Whether `value` passes.
    pub fn matches(&self, value: &[u8]) -> bool {
        let eq = |a: &[u8], b: &[u8]| {
            if self.ignore_case {
                a.eq_ignore_ascii_case(b)
            } else {
                a == b
            }
        };
        match &self.pattern {
            Pattern::Exact(p) => eq(value, p),
            Pattern::Prefix(p) => value.len() >= p.len() && eq(&value[..p.len()], p),
            Pattern::Suffix(p) => value.len() >= p.len() && eq(&value[value.len() - p.len()..], p),
            Pattern::Contains(re) | Pattern::Regex(re) => re.is_match(value),
        }
    }
}

/// What a [`HeaderMatcher`] or [`QueryMatcher`] asks of its value.
#[derive(Debug, Clone)]
pub enum ValueTest {
    /// The header or parameter is there, whatever its value.
    Present,
    /// The header or parameter is not there.
    Absent,
    /// The value passes a string test.
    Value(StringMatcher),
    /// The value is a decimal integer (optionally signed) in the range,
    /// start inclusive and end exclusive, as Envoy's `range_match`.
    Range(Range<i64>),
}

impl ValueTest {
    /// The test's answer for a value that is there (`Some`) or not.
    fn eval(&self, value: Option<&[u8]>, invert: bool, missing_as_empty: bool) -> bool {
        let value = match (self, value) {
            (Self::Present, v) => return v.is_some() != invert,
            (Self::Absent, v) => return v.is_none() != invert,
            (_, Some(v)) => v,
            (_, None) if missing_as_empty => b"",
            (_, None) => return false,
        };
        let pass = match self {
            Self::Value(m) => m.matches(value),
            Self::Range(range) => std::str::from_utf8(value)
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .is_some_and(|n| range.contains(&n)),
            Self::Present | Self::Absent => unreachable!("answered above"),
        };
        pass != invert
    }
}

/// A condition on one request header.
#[derive(Debug, Clone)]
pub struct HeaderMatcher {
    name: HeaderName,
    test: ValueTest,
    invert: bool,
    missing_as_empty: bool,
}

impl HeaderMatcher {
    /// A matcher on header `name`, in any case. Fails for a name that is
    /// not a valid header name.
    pub fn new(name: &str, test: ValueTest) -> Result<Self, MatchError> {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| MatchError(format!("invalid header name {name:?}")))?;
        Ok(Self {
            name,
            test,
            invert: false,
            missing_as_empty: false,
        })
    }

    /// Inverts the test (see the module docs for missing headers).
    pub fn invert(mut self, invert: bool) -> Self {
        self.invert = invert;
        self
    }

    /// Tests a missing header as if it were sent empty.
    pub fn missing_as_empty(mut self, missing_as_empty: bool) -> Self {
        self.missing_as_empty = missing_as_empty;
        self
    }

    /// The header this matcher reads.
    pub fn name(&self) -> &HeaderName {
        &self.name
    }

    /// Whether `headers` pass.
    pub fn matches(&self, headers: &HeaderMap) -> bool {
        let value = joined(headers, &self.name);
        self.test
            .eval(value.as_deref(), self.invert, self.missing_as_empty)
    }
}

/// All of a header's values joined with `,`, borrowed when it was sent
/// once.
fn joined<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<Cow<'a, [u8]>> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?.as_bytes();
    let Some(second) = values.next() else {
        return Some(Cow::Borrowed(first));
    };
    let mut all = first.to_vec();
    for v in std::iter::once(second).chain(values) {
        all.push(b',');
        all.extend_from_slice(v.as_bytes());
    }
    Some(Cow::Owned(all))
}

/// A condition on one query parameter.
#[derive(Debug, Clone)]
pub struct QueryMatcher {
    name: Box<[u8]>,
    test: ValueTest,
    invert: bool,
    missing_as_empty: bool,
}

impl QueryMatcher {
    /// A matcher on the parameter named exactly `name` (case-sensitive,
    /// compared after percent-decoding).
    pub fn new(name: &str, test: ValueTest) -> Self {
        Self {
            name: name.as_bytes().into(),
            test,
            invert: false,
            missing_as_empty: false,
        }
    }

    /// Inverts the test (see the module docs for missing parameters).
    pub fn invert(mut self, invert: bool) -> Self {
        self.invert = invert;
        self
    }

    /// Tests a missing parameter as if it were given empty.
    pub fn missing_as_empty(mut self, missing_as_empty: bool) -> Self {
        self.missing_as_empty = missing_as_empty;
        self
    }

    /// Whether `query` passes.
    pub fn matches(&self, query: &Query<'_>) -> bool {
        self.test
            .eval(query.first(&self.name), self.invert, self.missing_as_empty)
    }
}

/// One query parameter's decoded name and value, borrowed from the request
/// when neither had an escape.
type Param<'a> = (Cow<'a, [u8]>, Cow<'a, [u8]>);

/// A parsed query string: its parameters in order, percent-decoded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query<'a> {
    params: Vec<Param<'a>>,
}

impl<'a> Query<'a> {
    /// Parses `raw`, the part of the request target after `?` (without the
    /// `?`).
    pub fn parse(raw: &'a [u8]) -> Self {
        let params = raw
            .split(|&b| b == b'&')
            .filter(|piece| !piece.is_empty())
            .map(|piece| {
                let (name, value) = match piece.iter().position(|&b| b == b'=') {
                    Some(at) => (&piece[..at], &piece[at + 1..]),
                    None => (piece, &b""[..]),
                };
                (percent_decode(name), percent_decode(value))
            })
            .collect();
        Self { params }
    }

    /// The first value of parameter `name`.
    pub fn first(&self, name: &[u8]) -> Option<&[u8]> {
        self.params
            .iter()
            .find(|(n, _)| n.as_ref() == name)
            .map(|(_, v)| v.as_ref())
    }

    /// The parameters in order, duplicates included.
    pub fn params(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.params.iter().map(|(n, v)| (n.as_ref(), v.as_ref()))
    }
}

/// Decodes `%XX` escapes, keeping malformed ones as they are. Borrows when
/// there is nothing to decode.
fn percent_decode(s: &[u8]) -> Cow<'_, [u8]> {
    if !s.contains(&b'%') {
        return Cow::Borrowed(s);
    }
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                s.get(i + 1).copied().and_then(hex),
                s.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    Cow::Owned(out)
}

/// The standard methods, each a bit in [`MethodMatcher`]'s mask.
static STANDARD: [Method; 9] = [
    Method::GET,
    Method::HEAD,
    Method::POST,
    Method::PUT,
    Method::DELETE,
    Method::CONNECT,
    Method::OPTIONS,
    Method::TRACE,
    Method::PATCH,
];

/// A set of request methods, compared case-sensitively.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MethodMatcher {
    standard: u16,
    extension: Vec<Method>,
}

impl MethodMatcher {
    /// A matcher for any of `methods`. Fails for a name that is not a
    /// valid method token.
    pub fn new<I, S>(methods: I) -> Result<Self, MatchError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut m = Self::default();
        for name in methods {
            let name = name.as_ref();
            let method = Method::from_bytes(name.as_bytes())
                .map_err(|_| MatchError(format!("invalid method {name:?}")))?;
            match STANDARD.iter().position(|s| *s == method) {
                Some(bit) => m.standard |= 1 << bit,
                None if !m.extension.contains(&method) => m.extension.push(method),
                None => {}
            }
        }
        Ok(m)
    }

    /// Whether `method` is in the set.
    pub fn matches(&self, method: &Method) -> bool {
        match STANDARD.iter().position(|s| s == method) {
            Some(bit) => self.standard & (1 << bit) != 0,
            None => self.extension.contains(method),
        }
    }
}

/// A test on the request path (without the query).
#[derive(Debug, Clone)]
pub struct PathMatcher {
    test: StringMatcher,
    kind: PathKind,
    pattern: Box<str>,
    ignore_case: bool,
}

/// How a [`PathMatcher`] compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// The whole path.
    Exact,
    /// A byte prefix of the path, as the prefix routes have always been.
    Prefix,
    /// The whole path matches the regex.
    Regex,
}

impl PathMatcher {
    /// Builds a path matcher. Paths are case-sensitive unless
    /// `ignore_case`; fails only for a bad regex.
    pub fn new(kind: PathKind, pattern: &str, ignore_case: bool) -> Result<Self, MatchError> {
        let string_kind = match kind {
            PathKind::Exact => StringKind::Exact,
            PathKind::Prefix => StringKind::Prefix,
            PathKind::Regex => StringKind::Regex,
        };
        Ok(Self {
            test: StringMatcher::new(string_kind, pattern, ignore_case)?,
            kind,
            pattern: pattern.into(),
            ignore_case,
        })
    }

    /// How the path is compared.
    pub fn kind(&self) -> PathKind {
        self.kind
    }

    /// The pattern as configured: the literal, or the regex before it was
    /// anchored.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// Whether ASCII case is ignored.
    pub fn ignore_case(&self) -> bool {
        self.ignore_case
    }

    /// Whether `path` passes.
    pub fn matches(&self, path: &[u8]) -> bool {
        self.test.matches(path)
    }
}

/// What the matchers read from a request. The query string is parsed the
/// first time a matcher asks for it, then kept for the other routes.
#[derive(Debug)]
pub struct RequestView<'a> {
    method: &'a Method,
    path: &'a [u8],
    raw_query: Option<&'a [u8]>,
    headers: &'a HeaderMap,
    query: OnceCell<Query<'a>>,
}

impl<'a> RequestView<'a> {
    /// A view of a request. `path` should be cleaned already (see
    /// [`crate::path::RequestTarget::clean_path`]); `raw_query` is the part
    /// after `?`.
    pub fn new(
        method: &'a Method,
        path: &'a [u8],
        raw_query: Option<&'a [u8]>,
        headers: &'a HeaderMap,
    ) -> Self {
        Self {
            method,
            path,
            raw_query,
            headers,
            query: OnceCell::new(),
        }
    }

    /// The request path, as given to [`RequestView::new`].
    pub fn path(&self) -> &'a [u8] {
        self.path
    }

    /// The parsed query string, empty when there is none.
    pub fn query(&self) -> &Query<'a> {
        self.query
            .get_or_init(|| self.raw_query.map(Query::parse).unwrap_or_default())
    }
}

/// All of a route's conditions beyond its host; it matches when each one
/// does. An empty `RequestMatch` matches every request.
#[derive(Debug, Clone, Default)]
pub struct RequestMatch {
    pub path: Option<PathMatcher>,
    pub method: Option<MethodMatcher>,
    pub headers: Vec<HeaderMatcher>,
    pub query: Vec<QueryMatcher>,
}

impl RequestMatch {
    /// Whether `req` meets every condition. The cheap tests run first and
    /// the query string is parsed only if a query matcher is reached.
    pub fn matches(&self, req: &RequestView<'_>) -> bool {
        self.method.as_ref().is_none_or(|m| m.matches(req.method))
            && self.path.as_ref().is_none_or(|p| p.matches(req.path))
            && self.headers.iter().all(|h| h.matches(req.headers))
            && (self.query.is_empty() || {
                let query = req.query();
                self.query.iter().all(|q| q.matches(query))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use proptest::prelude::*;

    /// (header, test, invert, missing_as_empty, request headers, want)
    type HeaderCase = (
        &'static str,
        ValueTest,
        bool,
        bool,
        Vec<(&'static str, &'static str)>,
        bool,
    );
    /// (method, path, query, headers, want)
    type RequestCase<'a> = (Method, &'a str, Option<&'a str>, &'a HeaderMap, bool);

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (n, v) in pairs {
            map.append(
                HeaderName::from_bytes(n.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    #[test]
    fn string_matchers() {
        use StringKind::*;
        // (kind, pattern, ignore_case, value, want)
        let cases: &[(StringKind, &str, bool, &str, bool)] = &[
            (Exact, "abc", false, "abc", true),
            (Exact, "abc", false, "ABC", false),
            (Exact, "abc", true, "ABC", true),
            (Exact, "AbC", true, "aBc", true),
            (Exact, "abc", false, "abcd", false),
            (Exact, "", false, "", true),
            (Exact, "ß", true, "SS", false), // ASCII folding only
            (Prefix, "/api", false, "/api/v1", true),
            (Prefix, "/api", false, "/API/v1", false),
            (Prefix, "/api", true, "/API/v1", true),
            (Prefix, "/api", false, "/ap", false),
            (Prefix, "", false, "anything", true),
            (Suffix, ".css", false, "/a/b.css", true),
            (Suffix, ".css", true, "/a/b.CSS", true),
            (Suffix, ".css", false, "/a/b.CSS", false),
            (Suffix, ".css", false, "css", false),
            (Contains, "bot", false, "Googlebot/2.1", true),
            (Contains, "bot", false, "GoogleBot/2.1", false),
            (Contains, "BOT", true, "googlebot/2.1", true),
            (Contains, "a.b", false, "axb", false), // a literal, not a regex
            (Contains, "a.b", false, "xa.bx", true),
            (Contains, "", false, "", true),
            (Regex, "v[0-9]+", false, "v12", true),
            (Regex, "v[0-9]+", false, "v12x", false), // full match
            (Regex, "v[0-9]+", false, "xv12", false),
            (Regex, "a|b", false, "ab", false), // the anchor covers both sides
            (Regex, "a|b", false, "b", true),
            (Regex, "abc", true, "AbC", true),
            (Regex, "(?i)abc", false, "ABC", true),
            (Regex, r"\d+", false, "٣", true), // unicode-perl classes
            (Regex, "é", true, "É", true),     // unicode-case folding
        ];
        for &(kind, pattern, ignore_case, value, want) in cases {
            let m = StringMatcher::new(kind, pattern, ignore_case).unwrap();
            assert_eq!(
                m.matches(value.as_bytes()),
                want,
                "{kind:?} {pattern:?} ignore_case={ignore_case} on {value:?}"
            );
        }
    }

    #[test]
    fn regex_runs_on_bytes() {
        let dot = StringMatcher::new(StringKind::Regex, "/a.", false).unwrap();
        assert!(dot.matches("/aé".as_bytes()));
        assert!(!dot.matches(b"/a\xff"), ". matches only UTF-8");
        let any = StringMatcher::new(StringKind::Regex, "/a(?-u:.)", false).unwrap();
        assert!(any.matches(b"/a\xff"));
        let contains = StringMatcher::new(StringKind::Contains, "\u{fffd}", false).unwrap();
        assert!(!contains.matches(b"\xff"));
    }

    #[test]
    fn bad_regexes_are_refused() {
        for bad in ["(", "[a-", "a{2,1}", r"\q"] {
            let err = StringMatcher::new(StringKind::Regex, bad, false).unwrap_err();
            assert!(err.to_string().contains("regex"), "{err}");
        }
        // A pattern that compiles to more than the limit.
        let huge = StringMatcher::new(StringKind::Regex, r"(\w{100}){100}", false);
        assert!(huge.unwrap_err().to_string().contains("size limit"));
        let path = PathMatcher::new(PathKind::Regex, "(", false);
        assert!(path.is_err());
    }

    #[test]
    fn header_matchers() {
        let present = || ValueTest::Present;
        let exact = |s| ValueTest::Value(StringMatcher::exact(s));
        // (header, test, invert, missing_as_empty, request headers, want)
        let cases: Vec<HeaderCase> = vec![
            (
                "x-canary",
                present(),
                false,
                false,
                vec![("x-canary", "")],
                true,
            ),
            ("x-canary", present(), false, false, vec![], false),
            ("x-canary", present(), true, false, vec![], true),
            (
                "x-canary",
                present(),
                true,
                false,
                vec![("x-canary", "1")],
                false,
            ),
            ("x-canary", ValueTest::Absent, false, false, vec![], true),
            (
                "x-canary",
                ValueTest::Absent,
                false,
                false,
                vec![("x-canary", "1")],
                false,
            ),
            (
                "x-canary",
                ValueTest::Absent,
                true,
                false,
                vec![("x-canary", "1")],
                true,
            ),
            (
                "X-Canary",
                exact("1"),
                false,
                false,
                vec![("x-canary", "1")],
                true,
            ),
            (
                "x-canary",
                exact("1"),
                false,
                false,
                vec![("X-CANARY", "1")],
                true,
            ),
            (
                "x-canary",
                exact("1"),
                false,
                false,
                vec![("x-canary", "2")],
                false,
            ),
            (
                "x-canary",
                exact("1"),
                true,
                false,
                vec![("x-canary", "2")],
                true,
            ),
            // A value test on a missing header fails, inverted or not.
            ("x-canary", exact("1"), false, false, vec![], false),
            ("x-canary", exact("1"), true, false, vec![], false),
            // Unless missing counts as empty.
            ("x-canary", exact(""), false, true, vec![], true),
            ("x-canary", exact("1"), true, true, vec![], true),
            ("x-canary", exact("1"), false, true, vec![], false),
            // Several values are joined with commas.
            (
                "x-tag",
                exact("a,b"),
                false,
                false,
                vec![("x-tag", "a"), ("x-tag", "b")],
                true,
            ),
            (
                "x-tag",
                exact("a"),
                false,
                false,
                vec![("x-tag", "a"), ("x-tag", "b")],
                false,
            ),
            (
                "x-tag",
                exact("a,b,c"),
                false,
                false,
                vec![("x-tag", "a"), ("x-tag", "b"), ("x-tag", "c")],
                true,
            ),
            (
                "x-tag",
                exact("a, b"),
                false,
                false,
                vec![("x-tag", "a, b")],
                true,
            ),
            // Ranges: start inclusive, end exclusive, decimal integers only.
            (
                "x-n",
                ValueTest::Range(10..20),
                false,
                false,
                vec![("x-n", "10")],
                true,
            ),
            (
                "x-n",
                ValueTest::Range(10..20),
                false,
                false,
                vec![("x-n", "19")],
                true,
            ),
            (
                "x-n",
                ValueTest::Range(10..20),
                false,
                false,
                vec![("x-n", "20")],
                false,
            ),
            (
                "x-n",
                ValueTest::Range(-5..0),
                false,
                false,
                vec![("x-n", "-5")],
                true,
            ),
            (
                "x-n",
                ValueTest::Range(0..20),
                false,
                false,
                vec![("x-n", "+7")],
                true,
            ),
            (
                "x-n",
                ValueTest::Range(0..20),
                false,
                false,
                vec![("x-n", "7.0")],
                false,
            ),
            (
                "x-n",
                ValueTest::Range(0..20),
                false,
                false,
                vec![("x-n", "0x7")],
                false,
            ),
            (
                "x-n",
                ValueTest::Range(0..20),
                false,
                false,
                vec![("x-n", "")],
                false,
            ),
            (
                "x-n",
                ValueTest::Range(0..20),
                true,
                false,
                vec![("x-n", "abc")],
                true,
            ),
            ("x-n", ValueTest::Range(0..20), false, false, vec![], false),
            ("x-n", ValueTest::Range(0..20), false, true, vec![], false),
            (
                "x-n",
                ValueTest::Range(i64::MIN..i64::MAX),
                false,
                false,
                vec![("x-n", "99999999999999999999")],
                false,
            ),
        ];
        for (i, (name, test, invert, missing_as_empty, sent, want)) in cases.into_iter().enumerate()
        {
            let m = HeaderMatcher::new(name, test)
                .unwrap()
                .invert(invert)
                .missing_as_empty(missing_as_empty);
            assert_eq!(
                m.matches(&headers(&sent)),
                want,
                "case {i}: {m:?} on {sent:?}"
            );
        }
    }

    #[test]
    fn header_values_need_not_be_utf8() {
        let mut map = HeaderMap::new();
        map.insert("x-raw", HeaderValue::from_bytes(b"caf\xe9").unwrap());
        let m = HeaderMatcher::new(
            "x-raw",
            ValueTest::Value(StringMatcher::new(StringKind::Prefix, "caf", false).unwrap()),
        )
        .unwrap();
        assert!(m.matches(&map));
        let range = HeaderMatcher::new("x-raw", ValueTest::Range(0..1)).unwrap();
        assert!(!range.matches(&map));
    }

    #[test]
    fn invalid_header_names_are_refused() {
        for bad in ["", "x canary", "x:y", "é"] {
            let err = HeaderMatcher::new(bad, ValueTest::Present).unwrap_err();
            assert!(err.to_string().contains("header name"), "{err}");
        }
        let m = HeaderMatcher::new("X-Mixed-Case", ValueTest::Present).unwrap();
        assert_eq!(m.name().as_str(), "x-mixed-case");
    }

    #[test]
    fn query_parsing() {
        // (raw query, expected parameters in order)
        let cases: &[(&str, &[(&str, &str)])] = &[
            ("", &[]),
            ("a=1", &[("a", "1")]),
            ("a=1&b=2", &[("a", "1"), ("b", "2")]),
            ("a=1&&b=2&", &[("a", "1"), ("b", "2")]),
            ("debug", &[("debug", "")]),
            ("a=", &[("a", "")]),
            ("=x", &[("", "x")]),
            ("a=b=c", &[("a", "b=c")]),
            ("a=1&a=2", &[("a", "1"), ("a", "2")]),
            ("na%6De=v%61l", &[("name", "val")]),
            ("a=%2F%2f", &[("a", "//")]),
            ("a=%26b%3D1", &[("a", "&b=1")]),
            ("a=x+y", &[("a", "x+y")]),
            ("a=100%", &[("a", "100%")]),
            ("a=%zz%4", &[("a", "%zz%4")]),
            ("a=%%41", &[("a", "%A")]),
        ];
        for &(raw, want) in cases {
            let q = Query::parse(raw.as_bytes());
            let got: Vec<_> = q.params().collect();
            let want: Vec<_> = want
                .iter()
                .map(|(n, v)| (n.as_bytes(), v.as_bytes()))
                .collect();
            assert_eq!(got, want, "parse({raw:?})");
        }
        let q = Query::parse(b"a=%e9");
        assert_eq!(q.first(b"a"), Some(&b"\xe9"[..]));
    }

    #[test]
    fn query_matchers() {
        let exact = |s| ValueTest::Value(StringMatcher::exact(s));
        let regex = |s| ValueTest::Value(StringMatcher::new(StringKind::Regex, s, false).unwrap());
        // (parameter, test, invert, missing_as_empty, raw query, want)
        let cases: Vec<(&str, ValueTest, bool, bool, &str, bool)> = vec![
            ("debug", ValueTest::Present, false, false, "debug", true),
            ("debug", ValueTest::Present, false, false, "debug=0", true),
            ("debug", ValueTest::Present, false, false, "x=debug", false),
            ("debug", ValueTest::Present, true, false, "", true),
            ("debug", ValueTest::Absent, false, false, "", true),
            ("debug", ValueTest::Absent, false, false, "a=1&debug", false),
            ("debug", ValueTest::Absent, true, false, "debug", true),
            ("v", exact("2"), false, false, "v=2", true),
            ("v", exact("2"), false, false, "V=2", false), // names are case-sensitive
            ("v", exact("2"), false, false, "v=2&v=3", true), // first value wins
            ("v", exact("3"), false, false, "v=2&v=3", false),
            ("v", exact("2"), true, false, "v=3", true),
            ("v", exact("2"), true, false, "", false),
            ("v", exact(""), false, false, "v", true),
            ("v", exact(""), false, true, "", true),
            ("v", exact("2"), true, true, "", true),
            (
                "user id",
                exact("a b"),
                false,
                false,
                "user%20id=a%20b",
                true,
            ),
            ("v", regex("[0-9]+"), false, false, "v=123", true),
            ("v", regex("[0-9]+"), false, false, "v=123a", false),
            ("n", ValueTest::Range(1..3), false, false, "n=2", true),
            ("n", ValueTest::Range(1..3), false, false, "n=3", false),
        ];
        for (i, (name, test, invert, missing_as_empty, raw, want)) in cases.into_iter().enumerate()
        {
            let m = QueryMatcher::new(name, test)
                .invert(invert)
                .missing_as_empty(missing_as_empty);
            assert_eq!(
                m.matches(&Query::parse(raw.as_bytes())),
                want,
                "case {i}: {m:?} on {raw:?}"
            );
        }
    }

    #[test]
    fn method_matchers() {
        let ext = Method::from_bytes(b"PURGE").unwrap();
        let lower = Method::from_bytes(b"get").unwrap();
        // (methods, request method, want)
        let cases: Vec<(Vec<&str>, Method, bool)> = vec![
            (vec!["GET"], Method::GET, true),
            (vec!["GET"], Method::HEAD, false),
            (vec!["GET", "HEAD"], Method::HEAD, true),
            (vec!["GET"], lower.clone(), false), // case-sensitive
            (vec!["get"], lower, true),
            (vec!["get"], Method::GET, false),
            (vec!["PURGE"], ext.clone(), true),
            (vec!["PURGE", "PURGE"], ext.clone(), true),
            (vec!["PURGE"], Method::POST, false),
            (vec!["POST"], ext, false),
            (vec![], Method::GET, false),
        ];
        for s in &STANDARD {
            let m = MethodMatcher::new([s.as_str()]).unwrap();
            for other in &STANDARD {
                assert_eq!(m.matches(other), s == other, "{s} on {other}");
            }
        }
        for (methods, method, want) in cases {
            let m = MethodMatcher::new(&methods).unwrap();
            assert_eq!(m.matches(&method), want, "{methods:?} on {method}");
        }
        let dup = MethodMatcher::new(["PURGE", "PURGE"]).unwrap();
        assert_eq!(dup.extension.len(), 1);
        for bad in ["", "GE T", "GET\n"] {
            let err = MethodMatcher::new([bad]).unwrap_err();
            assert!(err.to_string().contains("invalid method"), "{err}");
        }
    }

    #[test]
    fn path_matchers() {
        use PathKind::*;
        // (kind, pattern, ignore_case, path, want)
        let cases: &[(PathKind, &str, bool, &[u8], bool)] = &[
            (Exact, "/login", false, b"/login", true),
            (Exact, "/login", false, b"/login/", false),
            (Exact, "/login", false, b"/LOGIN", false),
            (Exact, "/login", true, b"/LOGIN", true),
            (Prefix, "/api/", false, b"/api/v1", true),
            (Prefix, "/api/", false, b"/api", false),
            (Prefix, "/api/", true, b"/API/v1", true),
            (Prefix, "/", false, b"\xff", false),
            (Prefix, "/", false, b"/\xff", true),
            (Regex, r"/users/[0-9]+", false, b"/users/42", true),
            (Regex, r"/users/[0-9]+", false, b"/users/42/posts", false),
            (
                Regex,
                r"/users/[0-9]+(/.*)?",
                false,
                b"/users/42/posts",
                true,
            ),
            (Regex, r"/static/.*\.css", true, b"/STATIC/a.CSS", true),
            (Regex, r"/static/.*\.css", false, b"/STATIC/a.CSS", false),
        ];
        for &(kind, pattern, ignore_case, path, want) in cases {
            let m = PathMatcher::new(kind, pattern, ignore_case).unwrap();
            assert_eq!(
                m.matches(path),
                want,
                "{kind:?} {pattern:?} ignore_case={ignore_case} on {:?}",
                String::from_utf8_lossy(path)
            );
        }
    }

    #[test]
    fn request_match_needs_every_condition() {
        let m = RequestMatch {
            path: Some(PathMatcher::new(PathKind::Prefix, "/api/", false).unwrap()),
            method: Some(MethodMatcher::new(["GET", "HEAD"]).unwrap()),
            headers: vec![HeaderMatcher::new("x-canary", ValueTest::Present).unwrap()],
            query: vec![QueryMatcher::new(
                "v",
                ValueTest::Value(StringMatcher::exact("2")),
            )],
        };
        let canary = headers(&[("x-canary", "1")]);
        let none = HeaderMap::new();
        // (method, path, query, headers, want)
        let cases: &[RequestCase] = &[
            (Method::GET, "/api/x", Some("v=2"), &canary, true),
            (Method::HEAD, "/api/x", Some("a=1&v=2"), &canary, true),
            (Method::POST, "/api/x", Some("v=2"), &canary, false),
            (Method::GET, "/web/x", Some("v=2"), &canary, false),
            (Method::GET, "/api/x", Some("v=2"), &none, false),
            (Method::GET, "/api/x", Some("v=1"), &canary, false),
            (Method::GET, "/api/x", None, &canary, false),
        ];
        for (method, path, query, hdrs, want) in cases {
            let view = RequestView::new(method, path.as_bytes(), query.map(str::as_bytes), hdrs);
            assert_eq!(m.matches(&view), *want, "{method} {path}?{query:?}");
        }
        let any = RequestMatch::default();
        let view = RequestView::new(&Method::DELETE, b"/", None, &none);
        assert!(any.matches(&view));
    }

    #[test]
    fn query_is_parsed_only_when_asked_and_only_once() {
        let none = HeaderMap::new();
        let view = RequestView::new(&Method::GET, b"/a", Some(b"v=2"), &none);
        let no_query = RequestMatch {
            path: Some(PathMatcher::new(PathKind::Prefix, "/", false).unwrap()),
            ..RequestMatch::default()
        };
        assert!(no_query.matches(&view));
        assert!(view.query.get().is_none(), "parsed without a query matcher");
        // A failed cheaper test also skips the parse.
        let wrong_path = RequestMatch {
            path: Some(PathMatcher::new(PathKind::Prefix, "/b", false).unwrap()),
            query: vec![QueryMatcher::new("v", ValueTest::Present)],
            ..RequestMatch::default()
        };
        assert!(!wrong_path.matches(&view));
        assert!(view.query.get().is_none());
        let with_query = RequestMatch {
            query: vec![QueryMatcher::new("v", ValueTest::Present)],
            ..RequestMatch::default()
        };
        assert!(with_query.matches(&view));
        let first = view.query() as *const Query<'_>;
        assert!(with_query.matches(&view));
        assert_eq!(first, view.query() as *const Query<'_>);
    }

    fn ascii_case_variant() -> impl Strategy<Value = (String, String)> {
        "[a-zA-Z0-9/._-]{0,12}".prop_flat_map(|s| {
            let n = s.len();
            (Just(s), proptest::collection::vec(any::<bool>(), n)).prop_map(|(s, flips)| {
                let v = s
                    .chars()
                    .zip(flips)
                    .map(|(c, f)| {
                        if f {
                            c.to_ascii_uppercase()
                        } else {
                            c.to_ascii_lowercase()
                        }
                    })
                    .collect();
                (s, v)
            })
        })
    }

    proptest! {
        /// Each literal kind with ignore_case agrees with lower-casing both
        /// sides and comparing exactly, and a regex of the escaped literal
        /// agrees with Exact.
        #[test]
        fn ignore_case_is_ascii_lowercase(
            (pattern, variant) in ascii_case_variant(),
            extra in "[a-zA-Z]{0,3}",
        ) {
            let value = format!("{extra}{variant}{extra}");
            let (lp, lv) = (pattern.to_ascii_lowercase(), value.to_ascii_lowercase());
            for (kind, want) in [
                (StringKind::Exact, lv == lp),
                (StringKind::Prefix, lv.starts_with(&lp)),
                (StringKind::Suffix, lv.ends_with(&lp)),
                (StringKind::Contains, lv.contains(&lp)),
            ] {
                let m = StringMatcher::new(kind, &pattern, true).unwrap();
                prop_assert_eq!(m.matches(value.as_bytes()), want, "{:?}", kind);
            }
            let exact = StringMatcher::new(StringKind::Exact, &pattern, false).unwrap();
            let re = StringMatcher::new(StringKind::Regex, &regex::escape(&pattern), false).unwrap();
            prop_assert_eq!(exact.matches(value.as_bytes()), re.matches(value.as_bytes()));
        }

        /// Inverting a test on a present value always flips it; on a
        /// missing one it flips only presence tests.
        #[test]
        fn invert_flips_present_values(value in "[ -~]{0,8}", pattern in "[a-c]{0,2}", kind in 0..4usize) {
            let test = match kind {
                0 => ValueTest::Present,
                1 => ValueTest::Absent,
                2 => ValueTest::Range(0..50),
                _ => ValueTest::Value(StringMatcher::new(StringKind::Contains, &pattern, false).unwrap()),
            };
            let v = Some(value.as_bytes());
            prop_assert_ne!(test.eval(v, false, false), test.eval(v, true, false));
            let flips_missing = test.eval(None, false, false) != test.eval(None, true, false);
            prop_assert_eq!(flips_missing, kind < 2);
        }

        /// Decoding a percent-encoded string gives back the bytes.
        #[test]
        fn percent_decoding_round_trips(bytes in proptest::collection::vec(any::<u8>(), 0..24)) {
            let encoded: String = bytes.iter().map(|b| format!("%{b:02X}")).collect();
            let decoded = percent_decode(encoded.as_bytes());
            prop_assert_eq!(decoded.as_ref(), &bytes[..]);
        }
    }
}
