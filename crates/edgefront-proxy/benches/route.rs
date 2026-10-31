//! Route lookup over 1,000 host and prefix routes.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use edgefront_proxy::route::Router;

/// 1,000 routes in the shapes a real edge carries: 200 exact hosts with
/// three prefixes each, 50 wildcard hosts with two each, and 300 prefixes
/// on the catch-all host.
fn thousand_routes() -> Vec<(String, String, String)> {
    let mut routes = Vec::with_capacity(1000);
    for i in 0..200 {
        let host = format!("svc{i}.example.com");
        for prefix in [
            "/".to_owned(),
            "/api/".to_owned(),
            format!("/api/v{}/", i % 7),
        ] {
            routes.push((host.clone(), prefix, format!("svc{i}")));
        }
    }
    for i in 0..50 {
        let host = format!("*.tenant{i}.example.net");
        for prefix in ["/", "/static/"] {
            routes.push((host.clone(), prefix.to_owned(), format!("tenant{i}")));
        }
    }
    for i in 0..300 {
        routes.push(("*".to_owned(), format!("/svc{i}/"), format!("shared{i}")));
    }
    assert_eq!(routes.len(), 1000);
    routes
}

fn lookup(c: &mut Criterion) {
    let router = Router::new(thousand_routes());
    let mut group = c.benchmark_group("route_1000");
    let cases = [
        ("exact_host", "svc123.example.com", "/api/v4/users/42"),
        (
            "wildcard_host",
            "eu.cdn.tenant42.example.net",
            "/static/app.css",
        ),
        ("catch_all", "unknown.example.org", "/svc299/users/42"),
        (
            "mixed_case_with_port",
            "SVC123.Example.com:8443",
            "/api/v4/users/42",
        ),
        ("no_route", "unknown.example.org", "/nothing/here"),
    ];
    for (name, host, path) in cases {
        assert!(
            name == "no_route" || router.route(host, path).is_some(),
            "{name}"
        );
        group.bench_function(name, |b| {
            b.iter(|| router.route(black_box(host), black_box(path)))
        });
    }
    group.finish();
}

criterion_group!(benches, lookup);
criterion_main!(benches);
