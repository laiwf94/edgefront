//! Building client responses from stored objects: Age, conditional
//! requests (RFC 9110 section 13) and byte ranges.
//!
//! Preconditions are evaluated in RFC 9110 section 13.2.2's order against
//! the stored representation: If-Match (strong comparison), then
//! If-Unmodified-Since when there is no If-Match, either failing with 412;
//! then If-None-Match, or If-Modified-Since without it, giving 304; then
//! Range, only when the answer would otherwise be 200, and only if any
//! If-Range holds. Several ranges are sent as `multipart/byteranges`
//! (section 14.6), each part with the representation's Content-Type and
//! its own Content-Range.

use std::time::SystemTime;

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderValue};
use http::{Method, Response, StatusCode};

use crate::headers::{EntityTag, EtagMatch, date_header, parse_http_date};
use crate::object::{CachedObject, SURROGATE_KEY};
use crate::range::{self, ByteRange};

pub use edgefront_core::body::{Body, BoxError, empty, full, text};

/// Fields a 304 carries (RFC 9110 section 15.4.5), plus Last-Modified.
const NOT_MODIFIED_FIELDS: [header::HeaderName; 7] = [
    header::CACHE_CONTROL,
    header::CONTENT_LOCATION,
    header::DATE,
    header::ETAG,
    header::EXPIRES,
    header::VARY,
    header::LAST_MODIFIED,
];

/// Answers a GET or HEAD from a stored object.
pub fn from_cache(
    obj: &CachedObject,
    method: &Method,
    req: &HeaderMap,
    now: SystemTime,
) -> Response<Body> {
    let age = HeaderValue::from(obj.freshness.current_age(now).as_secs());
    // Conditionals and ranges only make sense for a 200 representation.
    if obj.status == StatusCode::OK {
        if precondition_failed(obj, req) {
            let mut resp = Response::new(empty());
            *resp.status_mut() = StatusCode::PRECONDITION_FAILED;
            resp.headers_mut().insert(header::AGE, age);
            resp.headers_mut()
                .insert(header::CONTENT_LENGTH, HeaderValue::from(0));
            return resp;
        }
        if not_modified(obj, req) {
            let mut resp = Response::new(empty());
            *resp.status_mut() = StatusCode::NOT_MODIFIED;
            for name in &NOT_MODIFIED_FIELDS {
                for value in obj.headers.get_all(name) {
                    resp.headers_mut().append(name.clone(), value.clone());
                }
            }
            resp.headers_mut().insert(header::AGE, age);
            return resp;
        }
        if method == Method::GET
            && let Some(resp) = ranged(obj, req, &age)
        {
            return resp;
        }
    }
    let mut headers = obj.headers.clone();
    headers.remove(SURROGATE_KEY);
    headers.insert(header::AGE, age);
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(obj.body.len()));
    if obj.status == StatusCode::OK {
        headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    }
    let body = if method == Method::HEAD {
        empty()
    } else {
        full(obj.body.clone())
    };
    let mut resp = Response::new(body);
    *resp.status_mut() = obj.status;
    *resp.headers_mut() = headers;
    resp
}

/// Whether If-Match, or If-Unmodified-Since without it, fails against the
/// stored representation (RFC 9110 sections 13.1.1, 13.1.4 and 13.2.2).
/// If-Match uses the strong comparison, so a weak stored tag never
/// matches a listed one, and `*` matches the stored representation. An
/// If-Match that does not parse is ignored. If-Unmodified-Since is ignored
/// when If-Match is present, when it is not one valid HTTP date, or when
/// the representation has no Last-Modified.
fn precondition_failed(obj: &CachedObject, req: &HeaderMap) -> bool {
    if req.contains_key(header::IF_MATCH) {
        return match EtagMatch::from_headers(req, &header::IF_MATCH) {
            Some(EtagMatch::Any) => false,
            Some(EtagMatch::Tags(tags)) => !obj
                .etag
                .as_ref()
                .is_some_and(|e| tags.iter().any(|t| t.strong_eq(e))),
            None => false,
        };
    }
    if req.get_all(header::IF_UNMODIFIED_SINCE).iter().count() != 1 {
        return false;
    }
    match (
        date_header(req, &header::IF_UNMODIFIED_SINCE),
        date_header(&obj.headers, &header::LAST_MODIFIED),
    ) {
        (Some(since), Some(modified)) => modified > since,
        _ => false,
    }
}

/// If-None-Match takes precedence; If-Modified-Since applies only without
/// it (RFC 9110 section 13.2.2).
fn not_modified(obj: &CachedObject, req: &HeaderMap) -> bool {
    if let Some(inm) = EtagMatch::from_headers(req, &header::IF_NONE_MATCH) {
        return inm.weak_matches(obj.etag.as_ref());
    }
    if req.contains_key(header::IF_NONE_MATCH) {
        return false;
    }
    // A stored response without Last-Modified is judged by its Date, or
    // failing that the time it was received (RFC 9111 section 4.3.2).
    let modified = date_header(&obj.headers, &header::LAST_MODIFIED)
        .or_else(|| date_header(&obj.headers, &header::DATE))
        .unwrap_or(obj.freshness.response_time);
    date_header(req, &header::IF_MODIFIED_SINCE).is_some_and(|since| modified <= since)
}

/// If-Range must name the stored representation exactly: a strong entity
/// tag or the Last-Modified date (RFC 9110 section 13.1.5).
fn if_range_holds(obj: &CachedObject, value: &HeaderValue) -> bool {
    if let Some(tag) = EntityTag::parse(value.as_bytes()) {
        return obj.etag.as_ref().is_some_and(|e| e.strong_eq(&tag));
    }
    match (
        parse_http_date(value),
        date_header(&obj.headers, &header::LAST_MODIFIED),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn ranged(obj: &CachedObject, req: &HeaderMap, age: &HeaderValue) -> Option<Response<Body>> {
    let range = req.get(header::RANGE)?;
    if let Some(cond) = req.get(header::IF_RANGE)
        && !if_range_holds(obj, cond)
    {
        return None;
    }
    let len = obj.body.len() as u64;
    let mut headers = obj.headers.clone();
    headers.remove(SURROGATE_KEY);
    headers.insert(header::AGE, age.clone());
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let (status, body) = match range::parse(range.as_bytes(), len) {
        ByteRange::Ignore => return None,
        ByteRange::Unsatisfiable => {
            let content_range = HeaderValue::from_str(&format!("bytes */{len}")).expect("ASCII");
            headers.insert(header::CONTENT_RANGE, content_range);
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(0));
            (StatusCode::RANGE_NOT_SATISFIABLE, Bytes::new())
        }
        ByteRange::Satisfiable { start, end } => {
            let content_range =
                HeaderValue::from_str(&format!("bytes {start}-{end}/{len}")).expect("ASCII");
            headers.insert(header::CONTENT_RANGE, content_range);
            let slice = obj.body.slice(start as usize..=end as usize);
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(slice.len()));
            (StatusCode::PARTIAL_CONTENT, slice)
        }
        ByteRange::Multiple(ranges) => {
            let boundary = boundary(obj, &ranges);
            let content_type = headers.remove(header::CONTENT_TYPE);
            let body = multipart(&obj.body, &ranges, &boundary, content_type.as_ref());
            let value =
                HeaderValue::from_str(&format!("multipart/byteranges; boundary={boundary}"))
                    .expect("ASCII");
            headers.insert(header::CONTENT_TYPE, value);
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
            (StatusCode::PARTIAL_CONTENT, body)
        }
    };
    let mut resp = Response::new(full(body));
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    Some(resp)
}

/// A boundary that cannot occur in the parts: 32 hex digits of a hash of
/// the body's bytes at the ranges and a process-wide counter, so two
/// responses never share one. RFC 2046 allows up to 70 characters.
fn boundary(obj: &CachedObject, ranges: &[(u64, u64)]) -> String {
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (n, obj.body.len(), ranges).hash(&mut h);
    let a = h.finish();
    obj.body.hash(&mut h);
    let b = h.finish();
    format!("edgefront-{a:016x}{b:016x}")
}

/// The `multipart/byteranges` body: each part's headers, its bytes, CRLF,
/// then the closing delimiter.
fn multipart(
    body: &Bytes,
    ranges: &[(u64, u64)],
    boundary: &str,
    content_type: Option<&HeaderValue>,
) -> Bytes {
    let len = body.len();
    let mut out = Vec::new();
    for &(start, end) in ranges {
        out.extend_from_slice(b"--");
        out.extend_from_slice(boundary.as_bytes());
        out.extend_from_slice(b"\r\n");
        if let Some(ct) = content_type {
            out.extend_from_slice(b"Content-Type: ");
            out.extend_from_slice(ct.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(
            format!("Content-Range: bytes {start}-{end}/{len}\r\n\r\n").as_bytes(),
        );
        out.extend_from_slice(&body[start as usize..=end as usize]);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"--");
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(b"--\r\n");
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::headers::http_date;
    use crate::object::tests::map;
    use crate::policy::Freshness;
    use http_body_util::BodyExt;

    fn t(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
    }

    fn obj_with(status: StatusCode, resp: &[(&str, &str)], body: &'static str) -> CachedObject {
        let f = Freshness {
            lifetime: Duration::from_secs(60),
            corrected_initial_age: Duration::from_secs(5),
            response_time: t(0),
        };
        CachedObject::new(
            status,
            map(resp),
            Bytes::from_static(body.as_bytes()),
            f,
            &HeaderMap::new(),
        )
    }

    fn obj(resp: &[(&str, &str)]) -> CachedObject {
        obj_with(StatusCode::OK, resp, "0123456789")
    }

    async fn body_of(resp: Response<Body>) -> Bytes {
        resp.into_body().collect().await.unwrap().to_bytes()
    }

    fn get(o: &CachedObject, req: &[(&str, &str)]) -> Response<Body> {
        from_cache(o, &Method::GET, &map(req), t(10))
    }

    #[tokio::test]
    async fn plain_hit() {
        let o = obj(&[
            ("etag", "\"v1\""),
            ("surrogate-key", "a b"),
            ("content-type", "text/plain"),
        ]);
        let resp = get(&o, &[]);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["age"], "15");
        assert_eq!(resp.headers()["content-length"], "10");
        assert_eq!(resp.headers()["accept-ranges"], "bytes");
        assert!(resp.headers().get("surrogate-key").is_none());
        assert_eq!(body_of(resp).await, "0123456789");
    }

    #[tokio::test]
    async fn head_has_headers_but_no_body() {
        let resp = from_cache(&obj(&[]), &Method::HEAD, &HeaderMap::new(), t(10));
        assert_eq!(resp.headers()["content-length"], "10");
        assert!(body_of(resp).await.is_empty());
    }

    #[test]
    fn if_none_match() {
        let o = obj(&[
            ("etag", "\"v1\""),
            ("cache-control", "max-age=60"),
            ("content-type", "text/plain"),
        ]);
        let resp = get(&o, &[("if-none-match", "W/\"v1\"")]);
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(resp.headers()["etag"], "\"v1\"");
        assert_eq!(resp.headers()["cache-control"], "max-age=60");
        assert!(resp.headers().get("content-type").is_none());
        assert_eq!(
            get(&o, &[("if-none-match", "\"v2\"")]).status(),
            StatusCode::OK
        );
        assert_eq!(
            get(&o, &[("if-none-match", "*")]).status(),
            StatusCode::NOT_MODIFIED
        );
    }

    #[test]
    fn if_modified_since() {
        let lm = http_date(t(0));
        let lm = lm.to_str().unwrap();
        let o = obj(&[("last-modified", lm)]);
        let later = http_date(t(100));
        let earlier = http_date(t(0) - Duration::from_secs(100));
        assert_eq!(
            get(&o, &[("if-modified-since", lm)]).status(),
            StatusCode::NOT_MODIFIED
        );
        assert_eq!(
            get(&o, &[("if-modified-since", later.to_str().unwrap())]).status(),
            StatusCode::NOT_MODIFIED
        );
        assert_eq!(
            get(&o, &[("if-modified-since", earlier.to_str().unwrap())]).status(),
            StatusCode::OK
        );
        assert_eq!(
            get(&o, &[("if-modified-since", "garbage")]).status(),
            StatusCode::OK
        );
        // If-None-Match wins, even when malformed.
        assert_eq!(
            get(&o, &[("if-none-match", "nope"), ("if-modified-since", lm)]).status(),
            StatusCode::OK
        );
        // Without Last-Modified, the stored Date stands in for it, and
        // without a Date, the time the response was received (t(0)).
        let dated = obj(&[("date", later.to_str().unwrap())]);
        assert_eq!(
            get(&dated, &[("if-modified-since", lm)]).status(),
            StatusCode::OK
        );
        assert_eq!(
            get(&dated, &[("if-modified-since", later.to_str().unwrap())]).status(),
            StatusCode::NOT_MODIFIED
        );
        assert_eq!(
            get(&obj(&[]), &[("if-modified-since", lm)]).status(),
            StatusCode::NOT_MODIFIED
        );
        assert_eq!(
            get(
                &obj(&[]),
                &[("if-modified-since", earlier.to_str().unwrap())]
            )
            .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn single_range() {
        let o = obj(&[]);
        let resp = get(&o, &[("range", "bytes=2-4")]);
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.headers()["content-range"], "bytes 2-4/10");
        assert_eq!(resp.headers()["content-length"], "3");
        assert_eq!(body_of(resp).await, "234");
        let resp = get(&o, &[("range", "bytes=-3")]);
        assert_eq!(body_of(resp).await, "789");
    }

    #[tokio::test]
    async fn unsatisfiable_and_ignored_ranges() {
        let o = obj(&[]);
        let resp = get(&o, &[("range", "bytes=10-")]);
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(resp.headers()["content-range"], "bytes */10");
        assert!(body_of(resp).await.is_empty());
        // Seventeen ranges: ignored, the whole object.
        let many = format!("bytes={}", vec!["0-0"; 17].join(","));
        let resp = get(&o, &[("range", many.as_str())]);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_of(resp).await, "0123456789");
        // HEAD ignores Range.
        let resp = from_cache(&o, &Method::HEAD, &map(&[("range", "bytes=0-1")]), t(10));
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn if_range() {
        let lm = http_date(t(0));
        let lm = lm.to_str().unwrap();
        let o = obj(&[("etag", "\"v1\""), ("last-modified", lm)]);
        let range = ("range", "bytes=0-1");
        assert_eq!(
            get(&o, &[range, ("if-range", "\"v1\"")]).status(),
            StatusCode::PARTIAL_CONTENT
        );
        assert_eq!(
            get(&o, &[range, ("if-range", "\"v2\"")]).status(),
            StatusCode::OK
        );
        assert_eq!(
            get(&o, &[range, ("if-range", "W/\"v1\"")]).status(),
            StatusCode::OK
        );
        assert_eq!(
            get(&o, &[range, ("if-range", lm)]).status(),
            StatusCode::PARTIAL_CONTENT
        );
        let other = http_date(t(1));
        assert_eq!(
            get(&o, &[range, ("if-range", other.to_str().unwrap())]).status(),
            StatusCode::OK
        );
        assert_eq!(
            get(&obj(&[]), &[range, ("if-range", "\"v1\"")]).status(),
            StatusCode::OK
        );
        assert_eq!(
            get(&obj(&[]), &[range, ("if-range", lm)]).status(),
            StatusCode::OK
        );
    }

    #[test]
    fn non_200_objects_ignore_conditionals_and_ranges() {
        let o = obj_with(StatusCode::NOT_FOUND, &[("etag", "\"v1\"")], "missing");
        let resp = get(&o, &[("if-none-match", "\"v1\""), ("range", "bytes=0-1")]);
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(resp.headers().get("accept-ranges").is_none());
    }

    #[tokio::test]
    async fn text_responses_are_not_cacheable() {
        let resp = text(StatusCode::BAD_GATEWAY, "origin unreachable\n");
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(resp.headers()["cache-control"], "no-store");
        assert_eq!(body_of(resp).await, "origin unreachable\n");
    }

    fn big(resp: &[(&str, &str)]) -> CachedObject {
        let f = Freshness {
            lifetime: Duration::from_secs(60),
            corrected_initial_age: Duration::ZERO,
            response_time: t(0),
        };
        let body: Vec<u8> = (0..4000u32).map(|i| b'a' + (i % 26) as u8).collect();
        CachedObject::new(
            StatusCode::OK,
            map(resp),
            Bytes::from(body),
            f,
            &HeaderMap::new(),
        )
    }

    #[tokio::test]
    async fn several_ranges_are_multipart() {
        let o = big(&[("content-type", "text/plain"), ("etag", "\"v1\"")]);
        let resp = get(&o, &[("range", "bytes=900-901,0-2")]);
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert!(resp.headers().get("content-range").is_none());
        let ct = resp.headers()["content-type"].to_str().unwrap().to_owned();
        let boundary = ct
            .strip_prefix("multipart/byteranges; boundary=")
            .unwrap()
            .to_owned();
        assert!(boundary.len() <= 70);
        let length: usize = resp.headers()["content-length"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let body = body_of(resp).await;
        assert_eq!(body.len(), length);
        let expected = format!(
            "--{boundary}\r\nContent-Type: text/plain\r\nContent-Range: bytes 900-901/4000\r\n\r\nqr\r\n--{boundary}\r\nContent-Type: text/plain\r\nContent-Range: bytes 0-2/4000\r\n\r\nabc\r\n--{boundary}--\r\n"
        );
        assert_eq!(std::str::from_utf8(&body).unwrap(), expected);
        // Two responses never share a boundary.
        let again = get(&o, &[("range", "bytes=900-901,0-2")]);
        assert_ne!(again.headers()["content-type"], ct.as_str());
        // Without a Content-Type, parts carry only Content-Range.
        let bare = big(&[]);
        let resp = get(&bare, &[("range", "bytes=0-0,500-500")]);
        let body = body_of(resp).await;
        assert!(!std::str::from_utf8(&body).unwrap().contains("Content-Type"));
    }

    #[tokio::test]
    async fn sixteen_ranges_are_served() {
        let o = big(&[]);
        let v: Vec<String> = (0..16)
            .map(|i| format!("{}-{}", i * 200, i * 200))
            .collect();
        let v = format!("bytes={}", v.join(","));
        let resp = get(&o, &[("range", v.as_str())]);
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        let body = body_of(resp).await;
        assert_eq!(
            std::str::from_utf8(&body)
                .unwrap()
                .matches("Content-Range:")
                .count(),
            16
        );
    }

    #[test]
    fn if_match_uses_the_strong_comparison() {
        let o = obj(&[("etag", "\"v1\"")]);
        assert_eq!(get(&o, &[("if-match", "\"v1\"")]).status(), StatusCode::OK);
        assert_eq!(
            get(&o, &[("if-match", "\"v0\", \"v1\"")]).status(),
            StatusCode::OK
        );
        assert_eq!(get(&o, &[("if-match", "*")]).status(), StatusCode::OK);
        let failed = get(&o, &[("if-match", "\"v2\"")]);
        assert_eq!(failed.status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(failed.headers()["content-length"], "0");
        // A weak tag never matches strongly, either side.
        assert_eq!(
            get(&o, &[("if-match", "W/\"v1\"")]).status(),
            StatusCode::PRECONDITION_FAILED
        );
        let weak = obj(&[("etag", "W/\"v1\"")]);
        assert_eq!(
            get(&weak, &[("if-match", "\"v1\"")]).status(),
            StatusCode::PRECONDITION_FAILED
        );
        // No stored tag: a listed tag cannot match; "*" still does.
        let untagged = obj(&[]);
        assert_eq!(
            get(&untagged, &[("if-match", "\"v1\"")]).status(),
            StatusCode::PRECONDITION_FAILED
        );
        assert_eq!(
            get(&untagged, &[("if-match", "*")]).status(),
            StatusCode::OK
        );
        // A malformed If-Match is ignored.
        assert_eq!(get(&o, &[("if-match", "v1")]).status(), StatusCode::OK);
        // HEAD too.
        let head = from_cache(&o, &Method::HEAD, &map(&[("if-match", "\"v2\"")]), t(10));
        assert_eq!(head.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[test]
    fn if_unmodified_since() {
        let lm = http_date(t(0));
        let lm = lm.to_str().unwrap();
        let o = obj(&[("last-modified", lm), ("etag", "\"v1\"")]);
        let later = http_date(t(100));
        let earlier = http_date(t(0) - Duration::from_secs(100));
        let (later, earlier) = (later.to_str().unwrap(), earlier.to_str().unwrap());
        assert_eq!(
            get(&o, &[("if-unmodified-since", lm)]).status(),
            StatusCode::OK
        );
        assert_eq!(
            get(&o, &[("if-unmodified-since", later)]).status(),
            StatusCode::OK
        );
        assert_eq!(
            get(&o, &[("if-unmodified-since", earlier)]).status(),
            StatusCode::PRECONDITION_FAILED
        );
        // Ignored beside If-Match, when invalid, when it is a list, and
        // when the representation has no Last-Modified.
        assert_eq!(
            get(
                &o,
                &[("if-match", "\"v1\""), ("if-unmodified-since", earlier)]
            )
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            get(&o, &[("if-unmodified-since", "yesterday")]).status(),
            StatusCode::OK
        );
        let list = format!("{earlier}, {earlier}");
        assert_eq!(
            get(&o, &[("if-unmodified-since", list.as_str())]).status(),
            StatusCode::OK
        );
        let mut two = map(&[("if-unmodified-since", earlier)]);
        two.append(
            "if-unmodified-since",
            HeaderValue::from_str(earlier).unwrap(),
        );
        assert_eq!(
            from_cache(&o, &Method::GET, &two, t(10)).status(),
            StatusCode::OK
        );
        assert_eq!(
            get(&obj(&[]), &[("if-unmodified-since", earlier)]).status(),
            StatusCode::OK
        );
    }

    #[test]
    fn preconditions_come_before_ranges_and_not_modified() {
        let o = obj(&[("etag", "\"v1\"")]);
        // A failed If-Match wins over If-None-Match and Range.
        let resp = get(
            &o,
            &[
                ("if-match", "\"v2\""),
                ("if-none-match", "\"v1\""),
                ("range", "bytes=0-1"),
            ],
        );
        assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
        // A passing one goes on to 304, and Range is ignored when 304.
        let resp = get(
            &o,
            &[
                ("if-match", "\"v1\""),
                ("if-none-match", "\"v1\""),
                ("range", "bytes=0-1"),
            ],
        );
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        let resp = get(&o, &[("if-match", "\"v1\""), ("range", "bytes=0-1")]);
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    }
}
