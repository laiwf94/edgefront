//! A warm restart of the segment store with a million objects.
//!
//! The store is filled, shut down cleanly, and opened again; opening must
//! come from the index snapshot and find every object. In a release build
//! (`cargo test --release -p edgefront-cache --test segment_restart`,
//! which the bench workflow runs) it holds a million objects and must be
//! ready in under a second. A debug build checks the same path with 50,000,
//! without the clock.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use edgefront_cache::object::{CachedObject, Entry, Key};
use edgefront_cache::policy::Freshness;
use edgefront_cache::store::segment::SegmentStore;
use http::{HeaderMap, HeaderValue, StatusCode};

#[test]
fn a_million_objects_restart_from_the_snapshot() {
    let count: usize = if cfg!(debug_assertions) {
        50_000
    } else {
        1_000_000
    };
    let dir =
        std::env::temp_dir().join(format!("edgefront-segment-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let freshness = Freshness {
        lifetime: Duration::from_secs(3600),
        corrected_initial_age: Duration::ZERO,
        response_time: SystemTime::now(),
    };
    let mut headers = HeaderMap::new();
    headers.insert("surrogate-key", HeaderValue::from_static("bulk"));
    let entry = Entry::single(Arc::new(CachedObject::new(
        StatusCode::OK,
        headers,
        Bytes::from_static(b"0123456789abcdef"),
        freshness,
        &HeaderMap::new(),
    )));
    {
        let store = SegmentStore::open(&dir, 8 << 30).unwrap();
        let keys: Vec<Key> = (0..count)
            .map(|i| format!("bench.example/obj/{i:07}").into())
            .collect();
        for chunk in keys.chunks(10_000) {
            let kept = store
                .put_many(chunk.iter().map(|k| (k.clone(), &entry)))
                .unwrap();
            assert_eq!(kept, chunk.len());
        }
        assert_eq!(store.len(), count);
        store.shutdown().unwrap();
    }
    let start = Instant::now();
    let store = SegmentStore::open(&dir, 8 << 30).unwrap();
    let ready = start.elapsed();
    assert!(store.warm, "the restart did not use the snapshot");
    assert_eq!(store.len(), count);
    for i in [0, count / 2, count - 1] {
        let got = store.get(&format!("bench.example/obj/{i:07}")).unwrap();
        assert_eq!(got.variants[0].body, "0123456789abcdef");
    }
    println!("{count} objects ready in {ready:?} from the snapshot");
    if !cfg!(debug_assertions) {
        assert!(
            ready < Duration::from_secs(1),
            "{count} objects took {ready:?}"
        );
    }
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
