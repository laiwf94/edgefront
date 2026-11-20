use super::*;
use http::{HeaderValue, Request};

fn spec(sel: &[&str]) -> KeySpec {
    KeySpec::new(sel.iter().map(|s| Selector::parse(s).unwrap()).collect())
}

/// The key `spec` gives a request for `target` with `headers`.
fn key_of(spec: &KeySpec, method: &str, target: &str, headers: &[(&str, &str)]) -> Option<Key> {
    let mut b = Request::builder().method(method).uri(target);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = b.body(()).unwrap();
    let pq = req.uri().path_and_query().map_or("/", |p| p.as_str());
    let t = edgefront_proxy::path::RequestTarget::parse(pq.as_bytes()).unwrap();
    spec.key(&View {
        method: req.method(),
        uri: req.uri(),
        headers: req.headers(),
        path: t.clean_path(),
    })
}

#[test]
fn selectors_parse_and_print_back() {
    for text in [
        "path",
        "path_prefix:/api",
        "path_prefix:/api/",
        "host",
        "method",
        "header:x-api-key",
        "cookie:session",
        "query:api_key",
    ] {
        assert_eq!(Selector::parse(text).unwrap().text(), text);
    }
    assert_eq!(
        Selector::parse("header:X-Api-Key").unwrap().text(),
        "header:x-api-key"
    );
    for (text, want) in [
        (
            "ip",
            "unknown key selector \"ip\"; the selectors are path, path_prefix:/PREFIX, host, method, header:NAME, cookie:NAME and query:NAME",
        ),
        (
            "header",
            "header needs a header name, as in \"header:x-api-key\"",
        ),
        (
            "header:",
            "header needs a header name, as in \"header:x-api-key\"",
        ),
        (
            "cookie:",
            "cookie needs a cookie name, as in \"cookie:session\"",
        ),
        (
            "query",
            "query needs a parameter name, as in \"query:api_key\"",
        ),
        (
            "path_prefix:",
            "path_prefix needs a path, as in \"path_prefix:/api\"",
        ),
        ("path:/x", "path takes no argument"),
        ("host:a", "host takes no argument"),
        ("path_prefix:api", "path_prefix \"api\" must start with /"),
        (
            "path_prefix:/a/../b",
            "path_prefix \"/a/../b\" is not a clean path; write \"/b\"",
        ),
        ("header:bad name", "\"bad name\" is not a header name"),
        ("cookie:a;b", "\"a;b\" is not a cookie name"),
    ] {
        assert_eq!(Selector::parse(text).unwrap_err(), want, "{text}");
    }
}

#[test]
fn every_field_keys_and_absent_fields_give_no_key() {
    let s = spec(&["path", "header:x-api-key"]);
    let a = key_of(&s, "GET", "/login", &[("x-api-key", "k1")]).unwrap();
    let b = key_of(&s, "GET", "/login", &[("x-api-key", "k2")]).unwrap();
    let c = key_of(&s, "GET", "/other", &[("x-api-key", "k1")]).unwrap();
    assert!(a != b && a != c && b != c);
    assert_eq!(
        a,
        key_of(&s, "POST", "/login?x=1", &[("x-api-key", " k1 ")]).unwrap()
    );
    assert_eq!(key_of(&s, "GET", "/login", &[]), None);

    let s = spec(&["method"]);
    assert_ne!(key_of(&s, "GET", "/", &[]), key_of(&s, "POST", "/", &[]));

    let s = spec(&["host"]);
    let h = key_of(&s, "GET", "/", &[("host", "Api.Example.com:8443")]).unwrap();
    assert_eq!(
        h,
        key_of(&s, "GET", "/", &[("host", "api.example.com.")]).unwrap()
    );
    assert_eq!(
        h,
        key_of(&s, "GET", "http://API.example.com/x", &[]).unwrap()
    );
    assert_eq!(key_of(&s, "GET", "/", &[]), None);
    let v6 = key_of(&s, "GET", "/", &[("host", "[::1]:80")]).unwrap();
    assert_eq!(v6, key_of(&s, "GET", "/", &[("host", "[::1]")]).unwrap());

    let s = spec(&["cookie:session"]);
    let c1 = key_of(&s, "GET", "/", &[("cookie", "a=1; session=abc; b=2")]).unwrap();
    assert_eq!(
        c1,
        key_of(&s, "GET", "/", &[("cookie", "session=\"abc\"")]).unwrap()
    );
    assert_ne!(
        c1,
        key_of(&s, "GET", "/", &[("cookie", "session=abd")]).unwrap()
    );
    assert_eq!(key_of(&s, "GET", "/", &[("cookie", "sessionx=abc")]), None);
    assert_eq!(key_of(&s, "GET", "/", &[("cookie", "nonsense")]), None);

    let s = spec(&["query:api_key"]);
    let q = key_of(&s, "GET", "/x?a=1&api_key=abc", &[]).unwrap();
    assert_eq!(q, key_of(&s, "GET", "/x?api_key=%61bc", &[]).unwrap());
    assert_eq!(q, key_of(&s, "GET", "/x?api%5Fkey=abc", &[]).unwrap());
    assert_ne!(q, key_of(&s, "GET", "/x?api_key=ab", &[]).unwrap());
    assert_eq!(
        key_of(&s, "GET", "/x?api_key=a+b", &[]),
        key_of(&s, "GET", "/x?api_key=a%20b", &[])
    );
    assert_eq!(key_of(&s, "GET", "/x?other=abc", &[]), None);
    assert_eq!(key_of(&s, "GET", "/x", &[]), None);
    // A bare name is an empty value; a malformed escape stays as it is.
    assert!(key_of(&s, "GET", "/x?api_key", &[]).is_some());
    assert_ne!(
        key_of(&s, "GET", "/x?api_key=%zz", &[]),
        key_of(&s, "GET", "/x?api_key=zz", &[])
    );

    // An empty spec: one key for everything.
    let s = spec(&[]);
    assert_eq!(key_of(&s, "GET", "/a", &[]), key_of(&s, "PUT", "/b", &[]));
}

#[test]
fn spellings_of_one_path_are_one_key() {
    let s = spec(&["path"]);
    let want = key_of(&s, "GET", "/login", &[]).unwrap();
    for t in [
        "/./login",
        "//login",
        "/%6Cogin",
        "/a/../login",
        "/login?next=/",
    ] {
        assert_eq!(key_of(&s, "GET", t, &[]).unwrap(), want, "{t}");
    }
    assert_ne!(key_of(&s, "GET", "/login/", &[]).unwrap(), want);
}

#[test]
fn a_prefix_covers_its_subtree_only() {
    let s = spec(&["path_prefix:/api", "header:x-key"]);
    let a = key_of(&s, "GET", "/api", &[("x-key", "1")]).unwrap();
    assert_eq!(
        a,
        key_of(&s, "GET", "/api/v1/users", &[("x-key", "1")]).unwrap()
    );
    assert_eq!(
        a,
        key_of(&s, "GET", "/x/../api/z", &[("x-key", "1")]).unwrap()
    );
    assert_eq!(key_of(&s, "GET", "/apix", &[("x-key", "1")]), None);
    assert_eq!(key_of(&s, "GET", "/", &[("x-key", "1")]), None);
    assert!(under_prefix(b"/api/", b"/api/"));
    assert!(under_prefix(b"/api/x", b"/api/"));
    assert!(!under_prefix(b"/api", b"/api/"));
    assert!(under_prefix(b"/anything", b"/"));
}

#[test]
fn repeated_fields_share_one_bucket() {
    let s = spec(&["header:x-api-key"]);
    let twice = key_of(
        &s,
        "GET",
        "/",
        &[("x-api-key", "junk1"), ("x-api-key", "real")],
    )
    .unwrap();
    let again = key_of(
        &s,
        "GET",
        "/",
        &[("x-api-key", "junk2"), ("x-api-key", "real")],
    )
    .unwrap();
    assert_eq!(twice, again, "a fresh first value is no fresh bucket");
    assert_ne!(
        twice,
        key_of(&s, "GET", "/", &[("x-api-key", "real")]).unwrap()
    );

    let s = spec(&["query:k"]);
    assert_eq!(
        key_of(&s, "GET", "/?k=a&k=b", &[]),
        key_of(&s, "GET", "/?k=c&k=d&k=e", &[])
    );
    let s = spec(&["cookie:s"]);
    assert_eq!(
        key_of(&s, "GET", "/", &[("cookie", "s=1"), ("cookie", "s=2")]),
        key_of(&s, "GET", "/", &[("cookie", "s=3; s=4")])
    );
    // No single value spells the marker: a decoded 0x00 0x02 is escaped.
    let s = spec(&["query:k"]);
    let marker = key_of(&s, "GET", "/?k=a&k=b", &[]).unwrap();
    assert_ne!(key_of(&s, "GET", "/?k=%00%02", &[]).unwrap(), marker);
    assert_ne!(key_of(&s, "GET", "/?k=%02", &[]).unwrap(), marker);
    assert_eq!(
        key_of(&s, "GET", "/?k=%00", &[]).unwrap().as_bytes(),
        b"\0\x01\0\0"
    );
}

#[test]
fn long_keys_are_hashed_and_still_distinct() {
    let s = spec(&["path", "header:authorization"]);
    let short = key_of(&s, "GET", "/a", &[("authorization", "b")]).unwrap();
    assert!(!short.is_hashed());
    assert_eq!(short.as_bytes(), b"/a\0\0b\0\0");
    let tok = "x".repeat(200);
    let long = key_of(&s, "GET", "/a", &[("authorization", &tok)]).unwrap();
    assert!(long.is_hashed());
    assert_eq!(long.as_bytes().len(), KEY_MAX);
    assert_eq!(
        long,
        key_of(&s, "GET", "/a", &[("authorization", &tok)]).unwrap()
    );
    let mut other = tok.clone();
    other.replace_range(199.., "y");
    assert_ne!(
        long,
        key_of(&s, "GET", "/a", &[("authorization", &other)]).unwrap()
    );
    // Exactly at the edge: 31 bytes stay inline, 32 are hashed.
    let s = spec(&["header:a"]);
    let fits = "v".repeat(INLINE_MAX - 2);
    assert!(!key_of(&s, "GET", "/", &[("a", &fits)]).unwrap().is_hashed());
    let over = "v".repeat(INLINE_MAX - 1);
    assert!(key_of(&s, "GET", "/", &[("a", &over)]).unwrap().is_hashed());
    // Debug never shows the bytes.
    assert_eq!(format!("{short:?}"), "Key { len: 7, hashed: false }");
    // Two specs hash with their own keys.
    let s2 = spec(&["path", "header:authorization"]);
    assert_ne!(
        Some(long),
        key_of(&s2, "GET", "/a", &[("authorization", &tok)])
    );
    assert_eq!(s, spec(&["header:a"]));
}

/// No two different value lists give one key, inline or hashed: random
/// header values under a two-header spec, against the pairs themselves.
#[test]
fn distinct_values_give_distinct_keys() {
    use proptest::prelude::*;
    let s = spec(&["header:a", "header:b"]);
    proptest!(ProptestConfig::with_cases(2000), |(
        a1 in proptest::collection::vec(1u8..=255, 0..40),
        b1 in proptest::collection::vec(1u8..=255, 0..40),
        a2 in proptest::collection::vec(1u8..=255, 0..40),
        b2 in proptest::collection::vec(1u8..=255, 0..40),
    )| {
        let mk = |a: &[u8], b: &[u8]| {
            let mut h = http::HeaderMap::new();
            let (Ok(a), Ok(b)) = (HeaderValue::from_bytes(a), HeaderValue::from_bytes(b)) else {
                return None;
            };
            h.insert("a", a);
            h.insert("b", b);
            let uri: http::Uri = "/".parse().unwrap();
            let v = View { method: &Method::GET, uri: &uri, headers: &h, path: b"/" };
            let trimmed = (h["a"].as_bytes().trim_ascii().to_vec(), h["b"].as_bytes().trim_ascii().to_vec());
            Some((s.key(&v).unwrap(), trimmed))
        };
        if let (Some((k1, v1)), Some((k2, v2))) = (mk(&a1, &b1), mk(&a2, &b2)) {
            prop_assert_eq!(k1 == k2, v1 == v2);
        }
    });
}
