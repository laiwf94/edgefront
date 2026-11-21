//! Compressed variants, dictionary compression and Early Hints (W07),
//! through the node's proxy in front of scripted origins.

mod common;

use std::time::Duration;

use common::{Harness, TestOrigin, handler, harness, reply};
use edgefront_cache::encoding::Coding;
use edgefront_cache::purge::PurgeTarget;
use edgefront_cache::variants::decompress;

/// A deterministic JavaScript bundle of about `len` bytes: version `v`
/// changes a few lines in every hundred, as a release of the same bundle
/// does.
pub fn bundle(v: u32, len: usize) -> String {
    let mut out = String::with_capacity(len + 128);
    let mut i = 0u32;
    while out.len() < len {
        let changed = if i.is_multiple_of(97) { v } else { 0 };
        out.push_str(&format!(
            "export function component{i}(props) {{ const state = useState({}); \
             return render(\"view-{}\", props.items.map((x) => x * {}), state); }}\n",
            i % 13 + changed,
            i % 31,
            i % 7 + 1
        ));
        i += 1;
    }
    out
}

fn compressing(origin: &TestOrigin) -> Harness {
    harness(&[origin.addr], |_, s| s.compress = true)
}

async fn js_origin(body: String) -> TestOrigin {
    TestOrigin::start(handler(move |_, _| {
        reply(
            200,
            &[
                ("content-type", "application/javascript"),
                ("cache-control", "max-age=60"),
                ("etag", "\"b1\""),
            ],
            body.clone(),
        )
    }))
    .await
}

async fn settled(h: &Harness) {
    tokio::time::timeout(Duration::from_secs(10), h.proxy.compressor().settled())
        .await
        .expect("compression finished");
}

#[tokio::test]
async fn text_is_stored_once_per_encoding_and_served_by_accept_encoding() {
    let body = bundle(1, 40 << 10);
    let origin = js_origin(body.clone()).await;
    let h = compressing(&origin);

    let first = h
        .get("/app.js", &[("accept-encoding", "gzip, deflate, br, zstd")])
        .await;
    assert_eq!(
        first.body,
        body.as_bytes(),
        "the fill answers with the source"
    );
    assert_eq!(first.header("vary"), "accept-encoding");
    settled(&h).await;

    let cases = [
        ("gzip, deflate, br, zstd", Coding::Br),
        ("br;q=1.0, gzip;q=0.5", Coding::Br),
        ("zstd", Coding::Zstd),
        ("gzip", Coding::Gzip),
        ("x-gzip;q=1, *;q=0", Coding::Gzip),
        ("", Coding::Identity),
        ("identity", Coding::Identity),
        ("deflate", Coding::Identity),
    ];
    let mut sizes = Vec::new();
    for (ae, want) in cases {
        let got = h.get("/app.js", &[("accept-encoding", ae)]).await;
        assert_eq!(got.status, 200, "{ae}");
        assert!(
            got.cache_status().contains("hit"),
            "{ae}: {}",
            got.cache_status()
        );
        assert_eq!(got.header("vary"), "accept-encoding", "{ae}");
        assert!(got.headers.get("edgefront-variant").is_none());
        let coding = match got.header("content-encoding") {
            "" => Coding::Identity,
            "br" => Coding::Br,
            "zstd" => Coding::Zstd,
            "gzip" => Coding::Gzip,
            other => panic!("{ae}: unexpected coding {other}"),
        };
        assert_eq!(coding, want, "{ae}");
        assert_eq!(got.header("content-length"), got.body.len().to_string());
        let plain = decompress(coding, &got.body, body.len()).unwrap();
        assert_eq!(plain, body.as_bytes(), "{ae}");
        if coding != Coding::Identity {
            assert_eq!(got.header("etag"), "W/\"b1\"", "{ae}");
        }
        sizes.push((ae, coding, got.body.len()));
    }
    // A client without Accept-Encoding gets identity.
    let none = h.get("/app.js", &[]).await;
    assert_eq!(none.header("content-encoding"), "");
    assert_eq!(none.body, body.as_bytes());
    // One origin fetch, four stored variants for every spelling.
    assert_eq!(origin.hits(), 1);
    let key = "example.com/app.js";
    assert_eq!(h.proxy.cache.memory().get(key).unwrap().variants.len(), 4);
    for (ae, coding, len) in &sizes {
        println!("{ae:>26} -> {:>8} {len:>6} bytes", coding.as_str());
    }
}

#[tokio::test]
async fn clients_accepting_nothing_held_go_to_the_origin() {
    let origin = js_origin(bundle(1, 8 << 10)).await;
    let h = compressing(&origin);
    h.get("/app.js", &[]).await;
    settled(&h).await;
    // Identity excluded and no coding held that the client names.
    let got = h
        .get("/app.js", &[("accept-encoding", "compress, identity;q=0")])
        .await;
    assert_eq!(origin.hits(), 2);
    assert_eq!(got.status, 200);
}

#[tokio::test]
async fn binary_small_and_no_transform_responses_are_not_compressed() {
    let origin = TestOrigin::start(handler(|parts, _| {
        let path = parts.uri.path();
        let (ct, cc, len) = match path {
            "/logo.png" => ("image/png", "max-age=60", 8 << 10),
            "/tiny.css" => ("text/css", "max-age=60", 100),
            _ => ("text/html", "max-age=60, no-transform", 8 << 10),
        };
        reply(
            200,
            &[("content-type", ct), ("cache-control", cc)],
            bundle(1, len),
        )
    }))
    .await;
    let h = compressing(&origin);
    for path in ["/logo.png", "/tiny.css", "/page.html"] {
        let first = h.get(path, &[("accept-encoding", "br")]).await;
        assert_eq!(first.header("vary"), "", "{path}");
        settled(&h).await;
        let again = h.get(path, &[("accept-encoding", "br")]).await;
        assert_eq!(again.header("content-encoding"), "", "{path}");
        let key = format!("example.com{path}");
        assert_eq!(
            h.proxy.cache.memory().get(&key).unwrap().variants.len(),
            1,
            "{path}"
        );
    }
}

#[tokio::test]
async fn routes_without_compress_store_what_the_origin_sent() {
    let origin = js_origin(bundle(1, 8 << 10)).await;
    let h = harness(&[origin.addr], |_, _| {});
    h.get("/app.js", &[("accept-encoding", "br")]).await;
    settled(&h).await;
    let again = h.get("/app.js", &[("accept-encoding", "br")]).await;
    assert_eq!(again.header("content-encoding"), "");
    assert_eq!(again.header("vary"), "");
    assert_eq!(
        h.proxy
            .cache
            .memory()
            .get("example.com/app.js")
            .unwrap()
            .variants
            .len(),
        1
    );
}

#[tokio::test]
async fn variants_are_purged_and_refreshed_together() {
    let origin = js_origin(bundle(1, 8 << 10)).await;
    let h = compressing(&origin);
    origin.use_clock(&h.clock);
    h.get("/app.js", &[]).await;
    settled(&h).await;
    let key = "example.com/app.js";
    assert_eq!(h.proxy.cache.memory().get(key).unwrap().variants.len(), 4);
    assert_eq!(
        h.proxy.cache.purge(&[PurgeTarget::Key(key.into())]).await,
        1
    );
    assert!(h.proxy.cache.memory().get(key).is_none());

    // A stale br copy is revalidated through its source: the origin gets
    // the source's validator, and the new source is compressed again.
    h.get("/app.js", &[]).await;
    settled(&h).await;
    h.clock.advance(Duration::from_secs(120));
    let stale = h.get("/app.js", &[("accept-encoding", "br")]).await;
    assert_eq!(stale.status, 200);
    assert_eq!(origin.last().headers["if-none-match"], "\"b1\"");
    settled(&h).await;
    let fresh = h.get("/app.js", &[("accept-encoding", "br")]).await;
    assert_eq!(fresh.header("content-encoding"), "br");
    assert!(fresh.cache_status().contains("hit"));
    assert_eq!(h.proxy.cache.memory().get(key).unwrap().variants.len(), 4);
}
