//! The hit path: what an edge cache does most.

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use cachet::cache_control::CacheControl;
use cachet::object::{CachedObject, Entry, Key};
use cachet::origin::{HealthSettings, Origin, OriginPool, client};
use cachet::policy::{self, Freshness};
use cachet::proxy::{Proxy, ProxySettings, SystemClock};
use cachet::store::memory::MemoryStore;
use cachet::store::{Cache, CacheSettings};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use http::{HeaderMap, HeaderValue, Request, StatusCode};
use http_body_util::{BodyExt, Empty};

fn object(body: usize) -> Arc<CachedObject> {
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
        Bytes::from(vec![b'x'; body]),
        freshness,
        &HeaderMap::new(),
    ))
}

fn parsing(c: &mut Criterion) {
    let value =
        "public, max-age=3600, s-maxage=600, stale-while-revalidate=30, stale-if-error=86400";
    c.bench_function("cache_control_parse", |b| {
        b.iter(|| CacheControl::parse_str(black_box(value)))
    });
    let obj = object(1024);
    let req = CacheControl::default();
    c.bench_function("freshness_decide", |b| {
        b.iter(|| policy::decide(black_box(&obj.freshness), &obj.cc, &req, SystemTime::now()))
    });
}

fn store(c: &mut Criterion) {
    let store = MemoryStore::new(256 << 20, 16, 8 << 20);
    let keys: Vec<Key> = (0..10_000)
        .map(|i| format!("example.com/page/{i}").into())
        .collect();
    for k in &keys {
        store.insert(k.clone(), Arc::new(Entry::single(object(1024))));
    }
    let mut i = 0;
    c.bench_function("memory_store_get", |b| {
        b.iter(|| {
            i = (i + 7919) % keys.len();
            black_box(store.get(&keys[i]))
        })
    });
}

fn full_hit(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let proxy = rt.block_on(async {
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
            object(4096),
            &HeaderMap::new(),
            cache.epoch(),
        );
        Proxy::new(cache, pool, ProxySettings::default(), Arc::new(SystemClock))
    });
    let peer = "192.0.2.1:4000".parse().unwrap();
    let mut group = c.benchmark_group("proxy");
    group.throughput(Throughput::Elements(1));
    group.bench_function("hit_4k", |b| {
        b.to_async(&rt).iter(|| async {
            let req = Request::builder()
                .uri("/hot")
                .header("host", "example.com")
                .body(Empty::<Bytes>::new())
                .unwrap();
            let resp = proxy.handle(req, peer).await;
            black_box(resp.into_body().collect().await.unwrap());
        })
    });
    group.finish();
}

criterion_group!(benches, parsing, store, full_hit);
criterion_main!(benches);
