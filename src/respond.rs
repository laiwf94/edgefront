//! Building client responses from stored objects: Age, conditional
//! requests (RFC 9110 section 13) and byte ranges.

use std::time::SystemTime;

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderValue};
use http::{Method, Response, StatusCode};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};

use crate::headers::{EntityTag, EtagMatch, date_header, parse_http_date};
use crate::object::{CachedObject, SURROGATE_KEY};
use crate::range::{self, ByteRange};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Body = BoxBody<Bytes, BoxError>;

pub fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}

pub fn empty() -> Body {
    Empty::new().map_err(|never| match never {}).boxed()
}

/// A plain-text response, for errors the proxy generates itself.
pub fn text(status: StatusCode, message: &'static str) -> Response<Body> {
    let mut resp = Response::new(full(message));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

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

/// If-None-Match takes precedence; If-Modified-Since applies only without
/// it (RFC 9110 section 13.2.2).
fn not_modified(obj: &CachedObject, req: &HeaderMap) -> bool {
    if let Some(inm) = EtagMatch::from_headers(req, &header::IF_NONE_MATCH) {
        return inm.weak_matches(obj.etag.as_ref());
    }
    if req.contains_key(header::IF_NONE_MATCH) {
        return false;
    }
    match (
        date_header(req, &header::IF_MODIFIED_SINCE),
        date_header(&obj.headers, &header::LAST_MODIFIED),
    ) {
        (Some(since), Some(modified)) => modified <= since,
        _ => false,
    }
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
    };
    let mut resp = Response::new(full(body));
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    Some(resp)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::headers::http_date;
    use crate::object::tests::map;
    use crate::policy::Freshness;

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
        assert_eq!(
            get(&obj(&[]), &[("if-modified-since", lm)]).status(),
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
        let resp = get(&o, &[("range", "bytes=0-1,4-5")]);
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
}
