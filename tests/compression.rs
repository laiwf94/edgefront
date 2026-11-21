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

/// The two releases of the fixture bundle (tests/fixtures/compression).
const APP_V1: &str = include_str!("fixtures/compression/app.v1.js");
const APP_V2: &str = include_str!("fixtures/compression/app.v2.js");

/// An origin serving the bundle's releases, v1 offered as a dictionary for
/// every later release.
async fn bundle_origin() -> TestOrigin {
    TestOrigin::start(handler(|parts, _| {
        let mut headers = vec![
            ("content-type", "text/javascript"),
            ("cache-control", "max-age=3600"),
        ];
        let body = match parts.uri.path() {
            "/js/app.v1.js" => {
                headers.push((
                    "use-as-dictionary",
                    r#"match="/js/app.*.js", match-dest=("script"), id="app""#,
                ));
                APP_V1
            }
            "/js/app.v2.js" => APP_V2,
            _ => return reply(404, &[], ""),
        };
        reply(200, &headers, body)
    }))
    .await
}

fn with_dictionary(dict: &str, ae: &str) -> Vec<(String, String)> {
    let hash = edgefront_cache::dictionary::hash(dict.as_bytes());
    vec![
        ("accept-encoding".into(), ae.into()),
        (
            "available-dictionary".into(),
            format!(":{}:", edgefront_cache::dictionary::base64(&hash)),
        ),
        ("dictionary-id".into(), "\"app\"".into()),
        ("sec-fetch-dest".into(), "script".into()),
    ]
}

async fn get_with(h: &Harness, path: &str, headers: &[(String, String)]) -> common::Got {
    let pairs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    h.get(path, &pairs).await
}

#[tokio::test]
async fn a_repeat_bundle_download_with_a_dictionary_is_a_fraction_of_brotli() {
    use edgefront_cache::dictionary::{DictCoding, decompress as dict_decompress};
    let origin = bundle_origin().await;
    let h = compressing(&origin);
    // The first visit loads v1, which offers itself as a dictionary.
    let v1 = h.get("/js/app.v1.js", &[("accept-encoding", "br")]).await;
    assert_eq!(v1.body, APP_V1.as_bytes());
    assert!(v1.header("use-as-dictionary").contains("app.*.js"));
    // The release after it, plain brotli for a client without v1.
    h.get("/js/app.v2.js", &[]).await;
    settled(&h).await;
    let br = h.get("/js/app.v2.js", &[("accept-encoding", "br")]).await;
    assert_eq!(br.header("content-encoding"), "br");
    // The same release for a client that holds v1.
    let mut sizes = Vec::new();
    for (ae, coding) in [
        ("gzip, br, zstd, dcb, dcz", DictCoding::Dcb),
        ("zstd, dcz", DictCoding::Dcz),
    ] {
        let got = get_with(&h, "/js/app.v2.js", &with_dictionary(APP_V1, ae)).await;
        assert_eq!(got.status, 200);
        assert_eq!(got.header("content-encoding"), coding.as_str());
        assert_eq!(got.header("vary"), "accept-encoding, available-dictionary");
        assert!(
            got.cache_status()
                .contains(&format!("detail={}", coding.as_str())),
            "{}",
            got.cache_status()
        );
        assert_eq!(got.header("content-length"), got.body.len().to_string());
        let plain = dict_decompress(coding, APP_V1.as_bytes(), &got.body, APP_V2.len()).unwrap();
        assert_eq!(plain, APP_V2.as_bytes());
        // A second request is answered from the kept body, byte for byte.
        let again = get_with(&h, "/js/app.v2.js", &with_dictionary(APP_V1, ae)).await;
        assert_eq!(again.body, got.body);
        sizes.push((coding, got.body.len()));
    }
    let saved = |n: usize| 100.0 * (1.0 - n as f64 / br.body.len() as f64);
    println!(
        "app.v2.js: identity {} bytes, br {} bytes, dcb {} bytes ({:.1}% under br), dcz {} bytes ({:.1}% under br)",
        APP_V2.len(),
        br.body.len(),
        sizes[0].1,
        saved(sizes[0].1),
        sizes[1].1,
        saved(sizes[1].1)
    );
    for (coding, n) in &sizes {
        assert!(saved(*n) >= 60.0, "{coding:?} saved only {:.1}%", saved(*n));
    }
    // One origin request per release, however it was asked for.
    assert_eq!(origin.hits(), 2);
}

#[tokio::test]
async fn requests_a_dictionary_does_not_apply_to_get_the_usual_coding() {
    let origin = bundle_origin().await;
    let h = compressing(&origin);
    h.get("/js/app.v1.js", &[]).await;
    h.get("/js/app.v2.js", &[]).await;
    settled(&h).await;
    let mut cases: Vec<(&str, Vec<(String, String)>)> = Vec::new();
    cases.push((
        "another dictionary",
        with_dictionary("not the bundle", "br, dcb"),
    ));
    cases.push(("no dictionary coding", with_dictionary(APP_V1, "br")));
    let mut style = with_dictionary(APP_V1, "br, dcb");
    style[3].1 = "style".into();
    cases.push(("another destination", style));
    let mut id = with_dictionary(APP_V1, "br, dcb");
    id[2].1 = "\"old\"".into();
    cases.push(("another id", id));
    let mut ranged = with_dictionary(APP_V1, "br, dcb");
    ranged.push(("range".into(), "bytes=0-99".into()));
    cases.push(("a range", ranged));
    for (what, headers) in cases {
        let got = get_with(&h, "/js/app.v2.js", &headers).await;
        assert!(
            matches!(got.header("content-encoding"), "br" | ""),
            "{what}: {}",
            got.header("content-encoding")
        );
        assert!(!got.cache_status().contains("detail=dc"), "{what}");
    }
}

/// What a raw HTTP/1.x client saw of one exchange: each response head in
/// order with when its first byte arrived, and the final body.
struct Exchange {
    heads: Vec<(u16, String, Duration)>,
    body: Vec<u8>,
}

/// Sends `request` on `conn` and reads interim responses and the final
/// one, timing each from when the request was written.
async fn exchange(conn: &mut tokio::net::TcpStream, request: &str) -> Exchange {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    conn.write_all(request.as_bytes()).await.unwrap();
    let sent = std::time::Instant::now();
    let mut buf: Vec<u8> = Vec::new();
    let mut heads = Vec::new();
    let mut seen = 0;
    let mut arrived = Vec::new();
    loop {
        // Every complete head in the buffer, timed by when its first byte
        // came in.
        while let Some(end) = buf[seen..].windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8(buf[seen..seen + end].to_vec()).unwrap();
            let status: u16 = head[9..12].parse().unwrap();
            // The read that brought the head's first byte.
            let at = arrived
                .iter()
                .rev()
                .find(|(offset, _): &&(usize, Duration)| *offset <= seen)
                .map_or(Duration::ZERO, |(_, t)| *t);
            heads.push((status, head.clone(), at));
            seen += end + 4;
            if status >= 200 {
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                while buf.len() < seen + len {
                    let mut chunk = [0u8; 65536];
                    let n = conn.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "closed in the body");
                    buf.extend_from_slice(&chunk[..n]);
                }
                return Exchange {
                    heads,
                    body: buf[seen..seen + len].to_vec(),
                };
            }
        }
        let mut chunk = [0u8; 65536];
        let n = tokio::time::timeout(Duration::from_secs(10), conn.read(&mut chunk))
            .await
            .expect("an answer")
            .unwrap();
        assert!(n > 0, "closed before the final response");
        arrived.push((buf.len(), sent.elapsed()));
        buf.extend_from_slice(&chunk[..n]);
    }
}

const PAGE_LINKS: [(&str, &str); 2] = [
    (
        "link",
        "</app.css>; rel=preload; as=style, </app.js>; rel=modulepreload",
    ),
    ("link", "</next.html>; rel=next"),
];

/// A node in front of an origin whose HTML page must be revalidated on
/// every request and takes `think` to answer.
async fn page_node(early_hints: bool, think: Duration) -> (TestOrigin, cachet::node::Node) {
    let origin = TestOrigin::start(handler(|parts, _| {
        if parts
            .headers
            .get("if-none-match")
            .is_some_and(|v| v == "\"p1\"")
        {
            return reply(
                304,
                &[("etag", "\"p1\""), ("cache-control", "no-cache")],
                "",
            );
        }
        let mut headers = vec![
            ("content-type", "text/html; charset=utf-8"),
            ("cache-control", "no-cache"),
            ("etag", "\"p1\""),
        ];
        headers.extend(PAGE_LINKS);
        reply(200, &headers, "<!doctype html><title>page</title>")
    }))
    .await;
    let extra = format!("[cache]\nearly_hints = {early_hints}\n");
    let config = common::node_config("edge-a", &[origin.addr], &extra);
    let node = cachet::node::start(&config, &common::secrets())
        .await
        .unwrap();
    // The first request stores the page; the delay starts after it.
    let mut conn = tokio::net::TcpStream::connect(node.proxy_addr)
        .await
        .unwrap();
    let first = exchange(&mut conn, "GET /page HTTP/1.1\r\nHost: example.com\r\n\r\n").await;
    assert_eq!(
        first.heads.len(),
        1,
        "nothing stored, so no hints: {:?}",
        first.heads
    );
    origin.set_delay(think);
    (origin, node)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_hints_arrive_before_the_origin_answers_a_revalidation() {
    let think = Duration::from_millis(300);
    let (origin, node) = page_node(true, think).await;
    let mut conn = tokio::net::TcpStream::connect(node.proxy_addr)
        .await
        .unwrap();
    // Twice on one connection: each request gets its own hints.
    for round in 0..2 {
        let got = exchange(&mut conn, "GET /page HTTP/1.1\r\nHost: example.com\r\n\r\n").await;
        let statuses: Vec<u16> = got.heads.iter().map(|h| h.0).collect();
        assert_eq!(statuses, [103, 200], "round {round}");
        let (_, hints, hinted_at) = &got.heads[0];
        assert!(hints.starts_with("HTTP/1.1 103 Early Hints\r\n"), "{hints}");
        assert!(
            hints.contains("link: </app.css>; rel=preload; as=style"),
            "{hints}"
        );
        assert!(
            hints.contains("link: </app.js>; rel=modulepreload"),
            "{hints}"
        );
        assert!(!hints.contains("rel=next"), "{hints}");
        let (_, last, final_at) = &got.heads[1];
        assert!(
            last.to_ascii_lowercase().contains("link: </app.css>"),
            "the final response keeps them"
        );
        assert_eq!(got.body, b"<!doctype html><title>page</title>");
        let gain = final_at.saturating_sub(*hinted_at);
        println!(
            "round {round}: 103 after {:.1} ms, 200 after {:.1} ms, first-byte gain {:.1} ms (origin think time {} ms)",
            hinted_at.as_secs_f64() * 1e3,
            final_at.as_secs_f64() * 1e3,
            gain.as_secs_f64() * 1e3,
            think.as_millis()
        );
        assert!(
            gain >= think,
            "gain {gain:?} under the think time {think:?}"
        );
    }
    assert_eq!(
        node.proxy
            .metrics
            .early_hints
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );
    assert_eq!(origin.hits(), 3);
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clients_that_cannot_take_hints_get_the_final_response_alone() {
    let think = Duration::from_millis(100);
    // HTTP/1.0 forbids 1xx responses.
    let (_origin, node) = page_node(true, think).await;
    let mut conn = tokio::net::TcpStream::connect(node.proxy_addr)
        .await
        .unwrap();
    let got = exchange(&mut conn, "GET /page HTTP/1.0\r\nHost: example.com\r\n\r\n").await;
    assert_eq!(got.heads.iter().map(|h| h.0).collect::<Vec<_>>(), [200]);
    assert!(
        got.heads[0].2 >= think,
        "the first byte waits for the origin"
    );
    node.shutdown().await;
    // With early_hints off, the first byte waits for the origin too.
    let (_origin, node) = page_node(false, think).await;
    let mut conn = tokio::net::TcpStream::connect(node.proxy_addr)
        .await
        .unwrap();
    let got = exchange(&mut conn, "GET /page HTTP/1.1\r\nHost: example.com\r\n\r\n").await;
    assert_eq!(got.heads.iter().map(|h| h.0).collect::<Vec<_>>(), [200]);
    assert!(got.heads[0].2 >= think);
    println!(
        "without hints the first byte came after {:.1} ms",
        got.heads[0].2.as_secs_f64() * 1e3
    );
    node.shutdown().await;
}
