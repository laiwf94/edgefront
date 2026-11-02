//! Maps a request's host and path to a route, ported from steer's
//! `internal/route`.
//!
//! Matching follows Envoy's virtual hosts: the host picks one virtual host
//! first (an exact name beats the longest `*.suffix` wildcard, which beats
//! `*`), then the longest path prefix inside that virtual host wins. There is
//! no fall-through to a less specific virtual host, so adding a route for one
//! host can never change where another host's requests go.
//!
//! The decisions are steer's, byte for byte; only the data structures
//! differ. steer scanned wildcard hosts and each virtual host's prefixes in
//! turn. Here exact and wildcard hosts are hash lookups (one per label of the
//! request host) and prefixes live in a radix trie, so a lookup does not slow
//! down as routes are added.

mod host;
mod prefix;

use std::collections::HashMap;

pub use host::normalize_host;
use prefix::PrefixTree;

/// An immutable routing table, safe to share between threads.
///
/// `T` is whatever a route resolves to: a cluster name, an index into the
/// config, or a whole route.
#[derive(Debug, Clone)]
pub struct Router<T> {
    /// Virtual hosts by exact host name.
    exact: HashMap<Box<str>, usize>,
    /// Wildcard virtual hosts by suffix, dot included: `*.example.com` is
    /// stored under `.example.com`.
    suffixes: HashMap<Box<str>, usize>,
    /// The shortest and longest key in `suffixes`, so a lookup only probes
    /// host suffixes that could be there.
    suffix_lens: (usize, usize),
    /// The `*` virtual host.
    any: Option<usize>,
    vhosts: Vec<PrefixTree<usize>>,
    routes: Vec<T>,
}

impl<T> Router<T> {
    /// Builds a router from `(host pattern, path prefix, target)` triples.
    ///
    /// A host pattern is `*`, `*.suffix` or an exact name, compared with the
    /// normalised request host as given, so patterns should already be
    /// lower case (config validation does that). Of two routes with the same
    /// host and prefix, the first wins.
    pub fn new<I, H, P>(routes: I) -> Self
    where
        I: IntoIterator<Item = (H, P, T)>,
        H: AsRef<str>,
        P: AsRef<str>,
    {
        let mut router = Self {
            exact: HashMap::new(),
            suffixes: HashMap::new(),
            suffix_lens: (usize::MAX, 0),
            any: None,
            vhosts: Vec::new(),
            routes: Vec::new(),
        };
        for (host, prefix, target) in routes {
            let host = host.as_ref();
            let next = router.vhosts.len();
            let slot = if host == "*" {
                router.any.get_or_insert(next)
            } else if let Some(suffix) = host.strip_prefix('*').filter(|s| s.starts_with('.')) {
                let (min, max) = &mut router.suffix_lens;
                (*min, *max) = ((*min).min(suffix.len()), (*max).max(suffix.len()));
                router.suffixes.entry(suffix.into()).or_insert(next)
            } else {
                router.exact.entry(host.into()).or_insert(next)
            };
            let vhost = *slot;
            if vhost == next {
                router.vhosts.push(PrefixTree::default());
            }
            router.vhosts[vhost].insert(prefix.as_ref().as_bytes(), router.routes.len());
            router.routes.push(target);
        }
        router
    }

    /// The route for a request. `host` is the Host header as sent (case,
    /// port, brackets and a trailing dot are ignored, see
    /// [`normalize_host`]); `path` should already be cleaned (see
    /// [`crate::path::RequestTarget::clean_path`]), since a prefix is
    /// matched byte for byte. A path may be any bytes, as a decoded `%ff`
    /// is.
    pub fn route(&self, host: &str, path: impl AsRef<[u8]>) -> Option<&T> {
        self.route_normalized(&normalize_host(host), path)
    }

    /// As [`Router::route`], for a host already passed through
    /// [`normalize_host`].
    pub fn route_normalized(&self, host: &str, path: impl AsRef<[u8]>) -> Option<&T> {
        let vhost = self.vhost(host)?;
        let index = *self.vhosts[vhost].longest_prefix(path.as_ref())?;
        Some(&self.routes[index])
    }

    fn vhost(&self, host: &str) -> Option<usize> {
        if let Some(&v) = self.exact.get(host) {
            return Some(v);
        }
        // A suffix matches when the host ends with it and has at least one
        // byte before it, so every candidate starts at a dot past the first
        // byte. Leftmost dot first means longest suffix first.
        let (min, max) = self.suffix_lens;
        for (at, _) in host.match_indices('.').filter(|&(at, _)| at > 0) {
            let len = host.len() - at;
            if len < min {
                break;
            }
            if len > max {
                continue;
            }
            if let Some(&v) = self.suffixes.get(&host[at..]) {
                return Some(v);
            }
        }
        self.any
    }

    /// The number of routes, duplicates included.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether the router has no routes, and so matches nothing.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// steer's TestMatch, case for case.
    #[test]
    fn steer_match_vectors() {
        let r = Router::new([
            ("*", "/", "default"),
            ("*", "/api/", "api"),
            ("*", "/api/v2/", "api-v2"),
            ("shop.example.com", "/", "shop"),
            ("*.example.com", "/", "wild"),
            ("*.eu.example.com", "/static/", "eu-static"),
        ]);
        let cases = [
            ("other.org", "/", Some("default")),
            ("other.org", "/api/users", Some("api")),
            ("other.org", "/api/v2/users", Some("api-v2")),
            ("other.org", "/api", Some("default")),
            ("SHOP.example.com:8443", "/api/x", Some("shop")),
            ("shop.example.com.", "/", Some("shop")),
            ("a.example.com", "/api/x", Some("wild")),
            ("a.b.example.com", "/", Some("wild")),
            ("example.com", "/", Some("default")),
            ("x.eu.example.com", "/static/a.css", Some("eu-static")),
            ("x.eu.example.com", "/index.html", None), // no fall-through
            ("[::1]:8080", "/api/", Some("api")),
            ("[::1]", "/", Some("default")),
        ];
        for (host, path, want) in cases {
            assert_eq!(
                r.route(host, path).copied(),
                want,
                "route({host:?}, {path:?})"
            );
        }
        let only_a = Router::new([("a.com", "/", "a")]);
        assert_eq!(
            only_a.route("b.com", "/"),
            None,
            "matched without a catch-all host"
        );
        assert_eq!(only_a.route("A.com", "/x"), Some(&"a"));
    }

    #[test]
    fn wildcard_needs_a_label_before_the_suffix() {
        let r = Router::new([("*.example.com", "/", 1), ("*", "/", 0)]);
        assert_eq!(r.route(".example.com", "/"), Some(&0));
        assert_eq!(r.route("example.com", "/"), Some(&0));
        assert_eq!(r.route("x.example.com", "/"), Some(&1));
        assert_eq!(r.route("..example.com", "/"), Some(&1));
    }

    #[test]
    fn longest_wildcard_suffix_wins() {
        let r = Router::new([
            ("*.com", "/", 1),
            ("*.b.com", "/", 2),
            ("*.a.b.com", "/", 3),
        ]);
        assert_eq!(r.route("x.com", "/"), Some(&1));
        assert_eq!(r.route("x.b.com", "/"), Some(&2));
        assert_eq!(r.route("a.b.com", "/"), Some(&2));
        assert_eq!(r.route("x.a.b.com", "/"), Some(&3));
        assert_eq!(r.route("com", "/"), None);
    }

    #[test]
    fn patterns_that_only_look_like_wildcards_are_exact() {
        let r = Router::new([("*x.com", "/", 1), ("**", "/", 2), ("*", "/", 3)]);
        assert_eq!(r.route("*x.com", "/"), Some(&1));
        assert_eq!(r.route("ax.com", "/"), Some(&3));
        assert_eq!(r.route("**", "/"), Some(&2));
    }

    #[test]
    fn first_of_duplicate_routes_wins() {
        let r = Router::new([
            ("a.com", "/x", 1),
            ("a.com", "/x", 2),
            ("*", "/", 3),
            ("*", "/", 4),
        ]);
        assert_eq!(r.route("a.com", "/x/y"), Some(&1));
        assert_eq!(r.route("b.com", "/x"), Some(&3));
        assert_eq!(r.len(), 4);
    }

    #[test]
    fn empty_router_matches_nothing() {
        let r: Router<u8> = Router::new(Vec::<(&str, &str, u8)>::new());
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert_eq!(r.route("a.com", "/"), None);
    }

    #[test]
    fn route_normalized_skips_normalisation() {
        let r = Router::new([("a.com", "/", 1)]);
        assert_eq!(r.route_normalized("a.com", "/"), Some(&1));
        assert_eq!(r.route_normalized("A.com", "/"), None);
        assert_eq!(r.route("A.com:1", "/"), Some(&1));
    }
}
