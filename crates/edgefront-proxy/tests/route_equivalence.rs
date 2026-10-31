//! The router against a brute-force matcher, ported from steer's
//! `reference` test helper: rank every route's host pattern against the
//! request host, keep the most specific rank, then the longest prefix.

use edgefront_proxy::route::{Router, normalize_host};
use proptest::prelude::*;

/// steer's reference matcher. Returns the index of the chosen route.
fn reference(routes: &[(String, String)], host: &str, path: &str) -> Option<usize> {
    let host = normalize_host(host);
    let rank = |pattern: &str| -> Option<usize> {
        if pattern == host {
            Some(1 << 20)
        } else if let Some(suffix) = pattern.strip_prefix('*').filter(|s| s.starts_with('.')) {
            (host.len() > suffix.len() && host.ends_with(suffix)).then_some(pattern.len())
        } else if pattern == "*" {
            Some(0)
        } else {
            None
        }
    };
    let best_rank = routes.iter().filter_map(|(h, _)| rank(h)).max()?;
    let mut best: Option<(usize, usize)> = None;
    for (i, (h, p)) in routes.iter().enumerate() {
        if rank(h) == Some(best_rank)
            && path.starts_with(p.as_str())
            && best.is_none_or(|(_, len)| p.len() > len)
        {
            best = Some((i, p.len()));
        }
    }
    best.map(|(i, _)| i)
}

const PATTERNS: &[&str] = &[
    "*",
    "a.com",
    "b.a.com",
    "*.a.com",
    "*.b.a.com",
    "c.org",
    "*.org",
    "*.",
    "*x.org",
];
const HOSTS: &[&str] = &[
    "a.com",
    "b.a.com",
    "x.b.a.com",
    "z.a.com",
    "c.org",
    "d.c.org",
    "e.net",
    "A.COM:80",
    "[b.a.com]:443",
    "B.A.COM.",
    "x.org",
    "*x.org",
    ".org",
    "org",
    "a..",
    "[x.org]",
    "",
    "*",
];
const SEGMENTS: &[&str] = &["api", "v1", "v2", "static", "x", "", "é"];

fn path() -> impl Strategy<Value = String> {
    (
        prop::collection::vec(prop::sample::select(SEGMENTS), 0..4),
        any::<bool>(),
    )
        .prop_map(|(segs, slash)| {
            let mut p: String = segs.iter().map(|s| format!("/{s}")).collect();
            if p.is_empty() || slash {
                p.push('/');
            }
            p
        })
}

fn pattern() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => prop::sample::select(PATTERNS).prop_map(str::to_owned),
        1 => "(\\*\\.)?[a-c.]{0,6}",
    ]
}

fn request_host() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => prop::sample::select(HOSTS).prop_map(str::to_owned),
        1 => "\\[?[a-cA-C.:\\[\\]*]{0,8}",
        1 => any::<String>(),
    ]
}

fn prefix() -> impl Strategy<Value = String> {
    prop_oneof![4 => path(), 1 => "[/a-c]{0,4}"]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 2000,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn router_matches_reference(
        routes in prop::collection::vec((pattern(), prefix()), 0..14),
        queries in prop::collection::vec((request_host(), path()), 1..40),
    ) {
        let router = Router::new(routes.iter().enumerate().map(|(i, (h, p))| (h, p, i)));
        for (host, path) in &queries {
            prop_assert_eq!(
                router.route(host, path).copied(),
                reference(&routes, host, path),
                "routes {:?} host {:?} path {:?}", routes, host, path
            );
        }
    }

    #[test]
    fn normalize_host_strips_port_case_and_one_dot(
        name in "[a-zA-Z0-9.-]{0,20}",
        port in prop::option::of("[0-9]{0,5}"),
    ) {
        let lower = name.to_ascii_lowercase();
        let want = lower.strip_suffix('.').unwrap_or(&lower);
        let host = match &port {
            Some(port) => format!("{name}:{port}"),
            None => name.clone(),
        };
        let bracketed = format!("[{name}]");
        let (plain, bracketed) = (normalize_host(&host), normalize_host(&bracketed));
        prop_assert_eq!(plain.as_ref(), want);
        prop_assert_eq!(bracketed.as_ref(), want);
    }
}
