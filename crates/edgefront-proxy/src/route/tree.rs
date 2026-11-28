//! Routes with full request matchers, compiled into a decision tree.
//!
//! A [`RouteTree`] answers the same question as trying every route in
//! config order and taking the first whose host and [`RequestMatch`] fit,
//! as Envoy's route table does, without trying every route:
//!
//! 1. **Host.** The request host picks one virtual host exactly as
//!    [`super::Router`] does (an exact name, else the longest `*.suffix`,
//!    else `*`), with no fall-through to a less specific one.
//! 2. **Path.** Inside the virtual host every route is filed under the
//!    literal bytes its path must start with: an exact path under itself in
//!    a hash map, a prefix in a radix trie, a regex under the literal text
//!    it begins with (see [`regex_literal_prefix`]), and a route with no
//!    path condition, or one that only a case-insensitive comparison can
//!    rule out, under the part before its first letter. A request then
//!    reaches only the routes filed under its exact path or one of its
//!    prefixes: one hash lookup and one walk down the trie.
//! 3. **The rest.** Each candidate list is in config order, and the routes
//!    in it are tested with [`RequestMatch::matches`] only while their
//!    position is below the best match found so far, so the answer is the
//!    first matching route in config order whatever list it came from.
//!
//! Filing is only ever conservative: a route sits under a prefix only when
//! no path outside that prefix could match it, so skipping the routes
//! filed elsewhere never changes the answer. The proptest below checks
//! exactly that against a linear first-match over random route sets.

use std::collections::HashMap;

use super::matcher::{PathKind, PathMatcher, RequestMatch, RequestView};
use super::prefix::PrefixTree;
use super::{HostIndex, normalize_host};

/// Positions of routes in config order, ascending.
type Candidates = Vec<u32>;

#[derive(Debug, Clone, Default)]
struct VirtualHost {
    /// Routes whose path must equal the key.
    exact: HashMap<Box<[u8]>, Candidates>,
    /// Routes whose path must start with the key.
    prefixes: PrefixTree<Candidates>,
}

/// Where a route is filed inside its virtual host.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Key {
    Exact(Box<[u8]>),
    Prefix(Box<[u8]>),
}

/// An immutable route table with header, query, method and regex
/// matchers, safe to share between threads. `T` is what a route resolves
/// to.
#[derive(Debug, Clone)]
pub struct RouteTree<T> {
    hosts: HostIndex,
    vhosts: Vec<VirtualHost>,
    routes: Vec<(RequestMatch, T)>,
}

impl<T> RouteTree<T> {
    /// Compiles `(host pattern, conditions, target)` routes, in config
    /// order. Host patterns are as for [`super::Router::new`]: `*`,
    /// `*.suffix` or an exact name, already lower case.
    ///
    /// # Panics
    ///
    /// With more than `u32::MAX` routes.
    pub fn new<I, H>(routes: I) -> Self
    where
        I: IntoIterator<Item = (H, RequestMatch, T)>,
        H: AsRef<str>,
    {
        let mut tree = Self {
            hosts: HostIndex::default(),
            vhosts: Vec::new(),
            routes: Vec::new(),
        };
        for (host, matcher, target) in routes {
            let at = u32::try_from(tree.routes.len()).expect("at most u32::MAX routes");
            let (vhost, new) = tree.hosts.insert(host.as_ref());
            if new {
                tree.vhosts.push(VirtualHost::default());
            }
            let vhost = &mut tree.vhosts[vhost];
            let list = match index_key(matcher.path.as_ref()) {
                Key::Exact(path) => vhost.exact.entry(path).or_default(),
                Key::Prefix(prefix) => vhost.prefixes.slot(&prefix).get_or_insert_default(),
            };
            list.push(at);
            tree.routes.push((matcher, target));
        }
        tree
    }

    /// The first route, in config order, that matches the request. `host`
    /// is the Host header as sent (see [`normalize_host`]).
    pub fn route(&self, host: &str, req: &RequestView<'_>) -> Option<&T> {
        self.route_normalized(&normalize_host(host), req)
    }

    /// As [`RouteTree::route`], for a host already passed through
    /// [`normalize_host`].
    pub fn route_normalized(&self, host: &str, req: &RequestView<'_>) -> Option<&T> {
        let at = self.route_index(host, req)?;
        Some(&self.routes[at].1)
    }

    /// The config position of the route [`RouteTree::route_normalized`]
    /// picks.
    pub fn route_index(&self, host: &str, req: &RequestView<'_>) -> Option<usize> {
        let vhost = &self.vhosts[self.hosts.get(host)?];
        let path = req.path();
        let mut best = u32::MAX;
        let mut scan = |list: &Candidates| {
            for &at in list {
                if at >= best {
                    return;
                }
                if self.routes[at as usize].0.matches(req) {
                    best = at;
                    return;
                }
            }
        };
        if let Some(list) = vhost.exact.get(path) {
            scan(list);
        }
        vhost.prefixes.for_each_prefix(path, &mut scan);
        (best != u32::MAX).then_some(best as usize)
    }

    /// The target of the route at config position `at`.
    pub fn get(&self, at: usize) -> Option<&T> {
        self.routes.get(at).map(|(_, t)| t)
    }

    /// The number of routes.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether there are no routes, and so nothing matches.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

/// Where a route with this path condition is filed.
fn index_key(path: Option<&PathMatcher>) -> Key {
    let Some(path) = path else {
        return Key::Prefix(Box::default());
    };
    let pattern = path.pattern();
    if path.ignore_case() {
        // Only the bytes before the first letter (or non-ASCII byte, which
        // a case-insensitive regex may fold) are compared as they are.
        let literal = match path.kind() {
            PathKind::Exact | PathKind::Prefix => pattern.to_owned(),
            PathKind::Regex => regex_literal_prefix(pattern),
        };
        let cut = literal
            .bytes()
            .position(|b| !b.is_ascii() || b.is_ascii_alphabetic())
            .unwrap_or(literal.len());
        return Key::Prefix(literal.as_bytes()[..cut].into());
    }
    match path.kind() {
        PathKind::Exact => Key::Exact(pattern.as_bytes().into()),
        PathKind::Prefix => Key::Prefix(pattern.as_bytes().into()),
        PathKind::Regex => Key::Prefix(regex_literal_prefix(pattern).into_bytes().into()),
    }
}

/// The characters [`regex::escape`] escapes: escaped, each stands for
/// itself. Other escapes (`\d`, `\<`, `\x41`, ...) are classes, assertions
/// or codes, and end the literal prefix.
const ESCAPED_LITERALS: &str = r"\.+*?()|[]{}^$#&-~";

/// Text that every string matching `pattern` as a whole (as
/// [`PathKind::Regex`] does, case-sensitively) starts with. It may be
/// shorter than the real one, never longer: it stops at the first
/// character that is not a plain or escaped literal, drops a literal that
/// a quantifier follows, and is empty for any pattern containing `|`,
/// which could be an alternation at the top level.
pub fn regex_literal_prefix(pattern: &str) -> String {
    let mut out = String::new();
    if pattern.contains('|') {
        return out;
    }
    // A leading `^` matches at the start of the value, where the anchored
    // pattern already is: no flag can come before it to make it multi-line.
    let mut chars = pattern
        .strip_prefix('^')
        .unwrap_or(pattern)
        .chars()
        .peekable();
    while let Some(c) = chars.next() {
        let literal = match c {
            '\\' => match chars.next() {
                Some(e) if ESCAPED_LITERALS.contains(e) => e,
                _ => break,
            },
            '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' => break,
            c => c,
        };
        if matches!(chars.peek(), Some('?' | '*' | '+' | '{')) {
            // This literal may repeat, or not be there at all.
            break;
        }
        out.push(literal);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::matcher::{
        HeaderMatcher, MethodMatcher, QueryMatcher, StringKind, StringMatcher, ValueTest,
    };
    use super::*;
    use http::{HeaderMap, HeaderName, HeaderValue, Method};
    use proptest::prelude::*;

    fn path(kind: PathKind, pattern: &str, ignore_case: bool) -> RequestMatch {
        RequestMatch {
            path: Some(PathMatcher::new(kind, pattern, ignore_case).unwrap()),
            ..RequestMatch::default()
        }
    }

    fn get<'a>(method: &'a Method, p: &'a str, headers: &'a HeaderMap) -> RequestView<'a> {
        let (p, q) = match p.split_once('?') {
            Some((p, q)) => (p, Some(q.as_bytes())),
            None => (p, None),
        };
        RequestView::new(method, p.as_bytes(), q, headers)
    }

    #[test]
    fn literal_prefixes() {
        let cases = [
            ("/api/v[0-9]+", "/api/v"),
            (r"/static/.*\.css", "/static/"),
            (r"/a\.b\-c\~d", "/a.b-c~d"),
            ("^/api", "/api"),
            ("^^/api", ""),
            ("/apis?", "/api"),
            ("/api*", "/ap"),
            ("/api+", "/ap"),
            ("/api{2}", "/ap"),
            ("/a|/b", ""),
            ("/a[|]", ""),
            ("/(a|b)", ""),
            ("(?i)/api", ""),
            ("/x(?i)abc", "/x"),
            (r"/\d+", "/"),
            (r"/\<word", "/"),
            (r"/\x41", "/"),
            ("/é/ü", "/é/ü"),
            ("/a b", "/a b"),
            ("/end$", "/end"),
            ("", ""),
            ("/", "/"),
            (r"\", ""),
        ];
        for (pattern, want) in cases {
            assert_eq!(regex_literal_prefix(pattern), want, "{pattern:?}");
        }
    }

    #[test]
    fn index_keys() {
        let key = |m: RequestMatch| index_key(m.path.as_ref());
        let prefix = |s: &str| Key::Prefix(s.as_bytes().into());
        assert_eq!(key(RequestMatch::default()), prefix(""));
        assert_eq!(
            key(path(PathKind::Exact, "/a", false)),
            Key::Exact(b"/a"[..].into())
        );
        assert_eq!(key(path(PathKind::Prefix, "/a", false)), prefix("/a"));
        assert_eq!(
            key(path(PathKind::Regex, "/a/[0-9]+", false)),
            prefix("/a/")
        );
        assert_eq!(key(path(PathKind::Exact, "/1/Ab", true)), prefix("/1/"));
        assert_eq!(key(path(PathKind::Prefix, "/1/é", true)), prefix("/1/"));
        assert_eq!(key(path(PathKind::Regex, "/9/k.*", true)), prefix("/9/"));
        assert_eq!(key(path(PathKind::Prefix, "/12", true)), prefix("/12"));
    }

    #[test]
    fn first_match_in_config_order_across_lists() {
        let mut beta = RequestMatch {
            headers: vec![HeaderMatcher::new("x-beta", ValueTest::Present).unwrap()],
            ..path(PathKind::Prefix, "/api/", false)
        };
        beta.method = Some(MethodMatcher::new(["GET"]).unwrap());
        let t = RouteTree::new([
            ("*", beta, "beta"),
            ("*", path(PathKind::Exact, "/api/health", false), "health"),
            (
                "*",
                path(PathKind::Regex, "/api/v[0-9]+/.*", false),
                "versioned",
            ),
            ("*", path(PathKind::Prefix, "/api/", false), "api"),
            ("*", path(PathKind::Prefix, "/API/", true), "api-any-case"),
            ("*", RequestMatch::default(), "default"),
            ("*", path(PathKind::Exact, "/api/late", false), "never"),
        ]);
        let none = HeaderMap::new();
        let mut beta_headers = HeaderMap::new();
        beta_headers.insert("x-beta", HeaderValue::from_static("1"));
        let cases = [
            (Method::GET, "/api/health", &beta_headers, Some("beta")),
            (Method::POST, "/api/health", &beta_headers, Some("health")),
            (Method::GET, "/api/health", &none, Some("health")),
            (Method::GET, "/api/v2/users", &none, Some("versioned")),
            (Method::GET, "/api/v2", &none, Some("api")),
            (Method::GET, "/Api/x", &none, Some("api-any-case")),
            (Method::GET, "/api/late", &none, Some("api")),
            (Method::GET, "/other", &none, Some("default")),
            (Method::GET, "", &none, Some("default")),
        ];
        for (method, p, headers, want) in cases {
            let view = get(&method, p, headers);
            assert_eq!(t.route("example.com", &view).copied(), want, "{method} {p}");
        }
        assert_eq!(t.len(), 7);
        assert!(!t.is_empty());
        assert_eq!(t.get(6), Some(&"never"));
        assert_eq!(t.get(7), None);
    }

    #[test]
    fn hosts_pick_one_virtual_host_without_fall_through() {
        let t = RouteTree::new([
            ("*", RequestMatch::default(), 0),
            (
                "shop.example.com",
                path(PathKind::Prefix, "/cart", false),
                1,
            ),
            ("*.example.com", RequestMatch::default(), 2),
            ("shop.example.com", path(PathKind::Exact, "/", false), 3),
        ]);
        let none = HeaderMap::new();
        let m = Method::GET;
        assert_eq!(
            t.route("SHOP.example.com:443", &get(&m, "/cart/1", &none)),
            Some(&1)
        );
        assert_eq!(t.route("shop.example.com", &get(&m, "/", &none)), Some(&3));
        assert_eq!(t.route("shop.example.com", &get(&m, "/x", &none)), None);
        assert_eq!(t.route("a.example.com", &get(&m, "/x", &none)), Some(&2));
        assert_eq!(t.route("example.com", &get(&m, "/x", &none)), Some(&0));
        assert_eq!(
            t.route_normalized("SHOP.example.com", &get(&m, "/", &none)),
            Some(&2)
        );
    }

    #[test]
    fn query_routes() {
        let canary = RequestMatch {
            query: vec![QueryMatcher::new(
                "v",
                ValueTest::Value(StringMatcher::new(StringKind::Exact, "2", false).unwrap()),
            )],
            ..path(PathKind::Prefix, "/", false)
        };
        let t = RouteTree::new([("*", canary, "v2"), ("*", RequestMatch::default(), "v1")]);
        let none = HeaderMap::new();
        let m = Method::GET;
        assert_eq!(t.route("a", &get(&m, "/x?v=2", &none)), Some(&"v2"));
        assert_eq!(t.route("a", &get(&m, "/x?v=3", &none)), Some(&"v1"));
        assert_eq!(t.route("a", &get(&m, "/x", &none)), Some(&"v1"));
    }

    #[test]
    fn empty_tree_matches_nothing() {
        let t: RouteTree<u8> = RouteTree::new(Vec::<(&str, RequestMatch, u8)>::new());
        let none = HeaderMap::new();
        assert!(t.is_empty());
        assert_eq!(t.route("a", &get(&Method::GET, "/", &none)), None);
    }

    /// A route as plain data, so proptest can print a failing case.
    #[derive(Debug, Clone)]
    struct Spec {
        host: &'static str,
        /// (0 exact, 1 prefix, 2 regex, pattern, ignore_case)
        path: Option<(u8, String, bool)>,
        methods: Option<Vec<&'static str>>,
        /// (0 present, 1 absent, 2 exact, 3 range, value, invert, missing_as_empty)
        header: Option<(u8, &'static str, bool, bool)>,
        query: Option<(u8, &'static str, bool, bool)>,
    }

    fn value_test(kind: u8, value: &str) -> ValueTest {
        match kind {
            0 => ValueTest::Present,
            1 => ValueTest::Absent,
            2 => ValueTest::Value(StringMatcher::exact(value)),
            _ => ValueTest::Range(0..5),
        }
    }

    impl Spec {
        fn build(&self) -> RequestMatch {
            let path = self.path.as_ref().map(|(kind, pattern, ic)| {
                let kind = [PathKind::Exact, PathKind::Prefix, PathKind::Regex][*kind as usize];
                PathMatcher::new(kind, pattern, *ic).unwrap()
            });
            let method = self
                .methods
                .as_ref()
                .map(|m| MethodMatcher::new(m).unwrap());
            let headers = self
                .header
                .iter()
                .map(|&(k, v, inv, mae)| {
                    HeaderMatcher::new("x-k", value_test(k, v))
                        .unwrap()
                        .invert(inv)
                        .missing_as_empty(mae)
                })
                .collect();
            let query = self
                .query
                .iter()
                .map(|&(k, v, inv, mae)| {
                    QueryMatcher::new("q", value_test(k, v))
                        .invert(inv)
                        .missing_as_empty(mae)
                })
                .collect();
            RequestMatch {
                path,
                method,
                headers,
                query,
            }
        }
    }

    const HOSTS: &[&str] = &[
        "*", "a.com", "*.a.com", "b.a.com", "*.com", "c.org", "*x.com",
    ];
    const REQUEST_HOSTS: &[&str] = &[
        "a.com",
        "b.a.com",
        "x.a.com",
        "x.b.a.com",
        "z.com",
        "c.org",
        "z.net",
        "*x.com",
        "yx.com",
    ];

    /// Paths over a small alphabet, so routes and requests collide often.
    fn path_text() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop::sample::select(vec!["/", "a", "b", "A", ".", "1"]),
            0..5,
        )
        .prop_map(|parts| parts.concat())
    }

    /// Regex pieces that mix literals with every construct the literal
    /// prefix stops at, each with texts it matches.
    const PIECES: &[(&str, &[&str])] = &[
        ("/", &["/"]),
        ("a", &["a"]),
        ("b", &["b"]),
        ("A", &["A"]),
        (r"\.", &["."]),
        ("1", &["1"]),
        (".", &["a", "/", "é"]),
        ("[ab]", &["a", "b"]),
        ("b?", &["", "b"]),
        ("a*", &["", "a", "aa"]),
        ("1+", &["1", "11"]),
        ("a{2}", &["aa"]),
        ("(a|b)", &["a", "b"]),
        ("/a|/b", &["/a", "/b"]),
        ("^", &[""]),
        ("(?i)a", &["a", "A"]),
        (r"\d", &["1"]),
        (".*", &["", "ab"]),
        ("$", &[""]),
    ];

    /// A regex built from [`PIECES`] and a text picked to match it (which
    /// it may not, where `^`, `$` or an alternation sit mid-pattern).
    fn regex_and_text() -> impl Strategy<Value = (String, String)> {
        proptest::collection::vec((0..PIECES.len(), 0..3usize), 0..5).prop_map(|parts| {
            let mut pattern = String::new();
            let mut text = String::new();
            for (piece, choice) in parts {
                let (p, texts) = PIECES[piece];
                pattern.push_str(p);
                text.push_str(texts[choice % texts.len()]);
            }
            (pattern, text)
        })
    }

    fn regex_text() -> impl Strategy<Value = String> {
        regex_and_text().prop_map(|(pattern, _)| pattern)
    }

    fn spec() -> impl Strategy<Value = Spec> {
        let path = prop_oneof![
            Just(None),
            (0..2u8, path_text(), any::<bool>()).prop_map(Some),
            (Just(2u8), regex_text(), any::<bool>()).prop_map(Some),
        ];
        let methods = prop_oneof![
            3 => Just(None),
            1 => Just(Some(vec!["GET"])),
            1 => Just(Some(vec!["POST", "PURGE"])),
        ];
        let value = || {
            proptest::option::weighted(
                0.3,
                (
                    0..4u8,
                    prop::sample::select(vec!["1", "a", ""]),
                    any::<bool>(),
                    any::<bool>(),
                ),
            )
        };
        (prop::sample::select(HOSTS), path, methods, value(), value()).prop_map(
            |(host, path, methods, header, query)| Spec {
                host,
                path,
                methods,
                header,
                query,
            },
        )
    }

    #[derive(Debug, Clone)]
    struct Req {
        host: &'static str,
        method: Method,
        path: String,
        query: Option<&'static str>,
        header: Option<&'static str>,
    }

    fn req() -> impl Strategy<Value = Req> {
        (
            prop::sample::select(REQUEST_HOSTS),
            prop::sample::select(vec![
                Method::GET,
                Method::POST,
                Method::from_bytes(b"PURGE").unwrap(),
            ]),
            path_text(),
            prop::sample::select(vec![
                None,
                Some("q=1"),
                Some("q"),
                Some("q=a&q=1"),
                Some("r=3"),
            ]),
            prop::sample::select(vec![None, Some("1"), Some("a"), Some("")]),
        )
            .prop_map(|(host, method, path, query, header)| Req {
                host,
                method,
                path,
                query,
                header,
            })
    }

    /// The reference: choose the virtual host by scanning every pattern,
    /// then try its routes one by one.
    fn naive(routes: &[(&str, RequestMatch)], host: &str, view: &RequestView<'_>) -> Option<usize> {
        let pattern = if routes.iter().any(|(h, _)| *h == host) {
            Some(host)
        } else {
            routes
                .iter()
                .map(|(h, _)| *h)
                .filter(|h| {
                    h.strip_prefix('*').is_some_and(|suffix| {
                        suffix.starts_with('.')
                            && host.len() > suffix.len()
                            && host.ends_with(suffix)
                    })
                })
                .max_by_key(|h| h.len())
                .or_else(|| routes.iter().map(|(h, _)| *h).find(|h| *h == "*"))
        }?;
        routes
            .iter()
            .position(|(h, m)| *h == pattern && m.matches(view))
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// The tree picks the same route as linear first-match.
        #[test]
        fn tree_equals_linear_first_match(
            specs in proptest::collection::vec(spec(), 0..24),
            reqs in proptest::collection::vec(req(), 1..24),
        ) {
            let built: Vec<(&str, RequestMatch)> =
                specs.iter().map(|s| (s.host, s.build())).collect();
            let tree = RouteTree::new(built.iter().cloned().enumerate().map(|(i, (h, m))| (h, m, i)));
            for r in &reqs {
                let mut headers = HeaderMap::new();
                if let Some(v) = r.header {
                    headers.insert(HeaderName::from_static("x-k"), HeaderValue::from_static(v));
                }
                let view = RequestView::new(&r.method, r.path.as_bytes(), r.query.map(str::as_bytes), &headers);
                let want = naive(&built, r.host, &view);
                prop_assert_eq!(tree.route_normalized(r.host, &view).copied(), want, "{:?}", r);
                prop_assert_eq!(tree.route_index(r.host, &view), want);
            }
        }

        /// Whatever matches a regex as a whole starts with its literal
        /// prefix, and with the part of it before any letter when the
        /// regex ignores case.
        #[test]
        fn literal_prefix_is_a_prefix_of_every_match(
            (pattern, text) in regex_and_text(),
            flips in proptest::collection::vec(any::<bool>(), 12),
            others in proptest::collection::vec(path_text(), 0..8),
        ) {
            let prefix = regex_literal_prefix(&pattern);
            let exact = PathMatcher::new(PathKind::Regex, &pattern, false).unwrap();
            let folded = PathMatcher::new(PathKind::Regex, &pattern, true).unwrap();
            let Key::Prefix(folded_prefix) = index_key(Some(&folded)) else {
                panic!("a regex is filed under a prefix");
            };
            let flipped: String = text
                .chars()
                .zip(flips.iter().cycle())
                .map(|(c, &f)| if f { c.to_ascii_uppercase() } else { c })
                .collect();
            for v in [&text, &flipped].into_iter().chain(&others) {
                if exact.matches(v.as_bytes()) {
                    prop_assert!(v.starts_with(&prefix), "{:?} matched {:?}", v, pattern);
                }
                if folded.matches(v.as_bytes()) {
                    prop_assert!(v.as_bytes().starts_with(&folded_prefix), "{:?} matched {:?}", v, pattern);
                }
            }
        }
    }
}
