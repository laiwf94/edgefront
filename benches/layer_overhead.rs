//! What the cache layer costs a request: on a route that bypasses it, and
//! for a hit through it. Results are in docs/benchmarks/layer.md.
//!
//! Every variant builds the same request and drains the same 4 KiB body,
//! so the differences between them are the pipeline's own cost.

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use cachet::cache_control::CacheControl;
use cachet::object::CachedObject;
use cachet::origin::{HealthSettings, Origin, OriginPool, client};
use cachet::policy::Freshness;
use cachet::proxy::{Proxy, ProxySettings};
use cachet::store::{Cache, CacheSettings};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use edgefront_cache::config::{RoutePolicies, RoutePolicy};
use edgefront_cache::layer::{CacheLayer, LayerSettings, SystemClock};
use edgefront_core::body::{Body, full};
use edgefront_core::filter::{self, Delivery, RouteFilters};
use http::{HeaderMap, HeaderValue, Request, Response, StatusCode};
use http_body_util::{BodyExt, Empty};

const BODY: Bytes = Bytes::from_static(&[b'x'; 4096]);

fn object() -> Arc<CachedObject> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "cache-control",
        HeaderValue::from_static("public, max-age=3600"),
    );
    headers.insert(
        "content-type",
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert("etag", HeaderValue::from_static("\"5f2a-9c1\""));
    let cc = CacheControl::from_headers(&headers);
    let now = SystemTime::now();
    let freshness = Freshness::compute(
        StatusCode::OK,
        &headers,
        &cc,
        now,
        now,
        Duration::from_secs(86_400),
    );
    Arc::new(CachedObject::new(
        StatusCode::OK,
        headers,
        BODY,
        freshness,
        &HeaderMap::new(),
    ))
}

fn request(path: &str) -> Request<Empty<Bytes>> {
    Request::builder()
        .uri(path)
        .header("host", "example.com")
        .body(Empty::new())
        .unwrap()
}

/// An upstream that answers at once with the 4 KiB body.
async fn upstream(_req: Request<Empty<Bytes>>) -> (Response<Body>, Delivery) {
    let mut resp = Response::new(full(BODY));
    resp.headers_mut()
        .insert("content-type", HeaderValue::from_static("text/html"));
    (resp, Delivery::Upstream)
}

async fn drain(resp: Response<Body>) {
    black_box(resp.into_body().collect().await.unwrap());
}

fn layer_overhead(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    // Route 0 is cached, route 1 is not.
    let cache = Arc::new(Cache::new(&CacheSettings::default()).unwrap());
    cache.store(
        "example.com/hot".into(),
        object(),
        &HeaderMap::new(),
        cache.epoch(),
    );
    let policy = RoutePolicy {
        max_object_bytes: 8 << 20,
        max_variants: 8,
        heuristic_max: Duration::from_secs(86_400),
        max_ttl: None,
    };
    let layer = CacheLayer::new(
        cache,
        RoutePolicies::new(vec![Some(policy), None]),
        LayerSettings::default(),
        Arc::new(SystemClock),
    );
    let filters: RouteFilters = layer.route_filters();
    let node = rt.block_on(async {
        let pool = Arc::new(OriginPool::new(
            vec![Arc::new(Origin::new(
                "unused",
                "127.0.0.1:9".parse().unwrap(),
            ))],
            client(Duration::from_secs(1)),
            Duration::from_secs(1),
            HealthSettings::default(),
        ));
        let cache = Cache::new(&CacheSettings::default()).unwrap();
        cache.store(
            "example.com/hot".into(),
            object(),
            &HeaderMap::new(),
            cache.epoch(),
        );
        Proxy::new(
            cache,
            pool,
            ProxySettings::default(),
            Arc::new(cachet::proxy::SystemClock),
        )
    });
    let peer = "192.0.2.1:4000".parse().unwrap();

    let mut group = c.benchmark_group("layer");
    group.throughput(Throughput::Elements(1));
    // The upstream called directly: the baseline for a bypassing route.
    group.bench_function("bypass/direct", |b| {
        b.to_async(&rt)
            .iter(|| async { drain(upstream(request("/cold")).await.0).await })
    });
    // What the proxy does for a route without a filter: one lookup in
    // the route's filter table, then the upstream.
    group.bench_function("bypass/route_filters", |b| {
        b.to_async(&rt).iter(|| async {
            let req = request("/cold");
            if let Some(f) = black_box(&filters).get(black_box(1)) {
                black_box(f);
            }
            drain(upstream(req).await.0).await
        })
    });
    // The same through `filter::run`, which a server without steps of its
    // own between the phases uses.
    group.bench_function("bypass/run", |b| {
        b.to_async(&rt).iter(|| async {
            drain(filter::run(filters.get(black_box(1)), 1, request("/cold"), upstream).await).await
        })
    });
    // A 4 KiB hit answered by the layer's request phase.
    group.bench_function("hit_4k/layer", |b| {
        b.to_async(&rt).iter(|| async {
            drain(filter::run(filters.get(black_box(0)), 0, request("/hot"), upstream).await).await
        })
    });
    // The same hit through cachet's node: its checks, the layer, the
    // request ID, the metrics and the access log.
    group.bench_function("hit_4k/node", |b| {
        b.to_async(&rt)
            .iter(|| async { drain(node.handle(request("/hot"), peer).await).await })
    });
    group.finish();
}

criterion_group!(benches, layer_overhead);
criterion_main!(benches);
