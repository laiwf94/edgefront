//! Shield tiers: per route, a set of upper-tier nodes that edge nodes send
//! their misses to instead of the origin.
//!
//! A route is a host (empty for any) and a path prefix, as the cache's own
//! routes are; the most specific match wins, a host match before any-host and
//! then the longest prefix. Within a route's set, the shield for a key is its
//! rendezvous owner among the shields that are alive, so every edge sends
//! one key to the same shield and that shield's request collapsing makes it
//! one origin request. A shield node never sends a miss to a shield; it goes
//! to the origin.

/// One route's shields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShieldRoute {
    /// The request host, lower case, or empty for any host.
    pub host: String,
    pub path_prefix: String,
    /// Node ids of the shields.
    pub nodes: Vec<String>,
}

/// Every route's shields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shields {
    routes: Vec<ShieldRoute>,
}

impl Shields {
    pub fn new(mut routes: Vec<ShieldRoute>) -> Self {
        for r in &mut routes {
            r.host.make_ascii_lowercase();
        }
        // Most specific first: a host before any host, then longer prefixes.
        routes.sort_by(|a, b| {
            b.host
                .is_empty()
                .cmp(&a.host.is_empty())
                .reverse()
                .then(b.path_prefix.len().cmp(&a.path_prefix.len()))
        });
        Shields { routes }
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// The shields for a request to `host` (port stripped by the caller or
    /// not; a `host:port` matches a route for `host`) and `path`.
    pub fn for_request(&self, host: &str, path: &str) -> Option<&[String]> {
        let host = host.rsplit_once(':').map_or(host, |(h, p)| {
            if p.bytes().all(|b| b.is_ascii_digit()) {
                h
            } else {
                host
            }
        });
        self.routes
            .iter()
            .find(|r| {
                (r.host.is_empty() || r.host.eq_ignore_ascii_case(host))
                    && path.starts_with(&r.path_prefix)
            })
            .map(|r| r.nodes.as_slice())
    }

    /// Whether `id` is a shield on any route.
    pub fn is_shield(&self, id: &str) -> bool {
        self.routes.iter().any(|r| r.nodes.iter().any(|n| n == id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(host: &str, prefix: &str, nodes: &[&str]) -> ShieldRoute {
        ShieldRoute {
            host: host.into(),
            path_prefix: prefix.into(),
            nodes: nodes.iter().map(|n| (*n).to_owned()).collect(),
        }
    }

    #[test]
    fn the_most_specific_route_wins() {
        let s = Shields::new(vec![
            route("", "/", &["any"]),
            route("", "/video/", &["video"]),
            route("Example.com", "/", &["example"]),
            route("example.com", "/video/live/", &["live"]),
        ]);
        assert_eq!(
            s.for_request("other.net", "/a"),
            Some(&["any".to_owned()][..])
        );
        assert_eq!(
            s.for_request("other.net", "/video/x"),
            Some(&["video".to_owned()][..])
        );
        assert_eq!(
            s.for_request("EXAMPLE.com:8080", "/video/x"),
            Some(&["example".to_owned()][..])
        );
        assert_eq!(
            s.for_request("example.com", "/video/live/1"),
            Some(&["live".to_owned()][..])
        );
        assert_eq!(
            s.for_request("[::1]:80", "/a"),
            Some(&["any".to_owned()][..])
        );
        assert!(s.is_shield("video") && !s.is_shield("edge-1"));
        let only = Shields::new(vec![route("", "/static/", &["s"])]);
        assert_eq!(only.for_request("h", "/dynamic"), None);
        assert!(Shields::default().is_empty() && !only.is_empty());
    }
}
