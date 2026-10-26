//! Stored responses, cache keys and Vary-aware variant selection
//! (RFC 9111 sections 2 and 4.1).

use std::sync::Arc;

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName};
use http::{StatusCode, Uri};

use crate::cache_control::CacheControl;
use crate::headers::{EntityTag, field_list};
use crate::policy::Freshness;

/// The primary cache key: lower-cased host followed by the request target.
pub type Key = Arc<str>;

/// Builds the primary key from the request target and Host header. Returns
/// `None` when neither names a host.
pub fn primary_key(uri: &Uri, headers: &HeaderMap) -> Option<Key> {
    let host = match headers.get(header::HOST).and_then(|h| h.to_str().ok()) {
        Some(h) => h.trim(),
        None => uri.authority()?.as_str(),
    };
    if host.is_empty() {
        return None;
    }
    let target = uri.path_and_query().map_or("/", |pq| pq.as_str());
    let mut key = String::with_capacity(host.len() + target.len());
    key.push_str(&host.to_ascii_lowercase());
    key.push_str(target);
    Some(key.into())
}

/// The selecting header values a stored response was chosen by.
pub type VaryValues = Vec<(HeaderName, Option<String>)>;

/// A stored response.
#[derive(Debug)]
pub struct CachedObject {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub freshness: Freshness,
    pub cc: CacheControl,
    pub etag: Option<EntityTag>,
    /// Surrogate keys this response can be purged by.
    pub tags: Vec<Arc<str>>,
    vary: VaryValues,
}

/// Rough per-object bookkeeping cost, so many tiny objects still count
/// against the byte budget.
const OBJECT_OVERHEAD: usize = 256;

impl CachedObject {
    /// Builds a stored response. `req_headers` are the headers of the
    /// request that produced it, used to record Vary selecting values.
    pub fn new(
        status: StatusCode,
        headers: HeaderMap,
        body: Bytes,
        freshness: Freshness,
        req_headers: &HeaderMap,
    ) -> Self {
        let vary = field_list(&headers, &header::VARY)
            .into_iter()
            .filter_map(|name| HeaderName::from_bytes(name.as_bytes()).ok())
            .map(|name| {
                let value = normalized(req_headers, &name);
                (name, value)
            })
            .collect();
        Self::with_vary(status, headers, body, freshness, vary)
    }

    /// Builds a stored response with already-recorded selecting values.
    pub fn with_vary(
        status: StatusCode,
        headers: HeaderMap,
        body: Bytes,
        freshness: Freshness,
        vary: VaryValues,
    ) -> Self {
        CachedObject {
            status,
            cc: CacheControl::from_headers(&headers),
            etag: EntityTag::from_header(&headers),
            tags: surrogate_keys(&headers),
            headers,
            body,
            freshness,
            vary,
        }
    }

    pub fn vary(&self) -> &VaryValues {
        &self.vary
    }

    /// Whether this response was selected by the same header values as `req`.
    pub fn matches(&self, req: &HeaderMap) -> bool {
        self.vary
            .iter()
            .all(|(name, value)| normalized(req, name) == *value)
    }

    /// Bytes charged against the cache budget.
    pub fn size(&self) -> usize {
        let headers: usize = self
            .headers
            .iter()
            .map(|(k, v)| k.as_str().len() + v.len() + 32)
            .sum();
        OBJECT_OVERHEAD + headers + self.body.len()
    }
}

/// The response header carrying surrogate keys, space separated.
pub const SURROGATE_KEY: HeaderName = HeaderName::from_static("surrogate-key");
pub const MAX_TAGS: usize = 64;
pub const MAX_TAG_LEN: usize = 128;

/// Reads surrogate keys from a response. Keys are visible ASCII without
/// spaces; overlong keys are dropped and at most `MAX_TAGS` are kept.
pub fn surrogate_keys(headers: &HeaderMap) -> Vec<Arc<str>> {
    let mut tags: Vec<Arc<str>> = Vec::new();
    for line in headers.get_all(SURROGATE_KEY) {
        let Ok(text) = line.to_str() else { continue };
        for tag in text.split_ascii_whitespace() {
            if tag.len() <= MAX_TAG_LEN && valid_tag(tag) && !tags.iter().any(|t| &**t == tag) {
                tags.push(tag.into());
            }
        }
    }
    tags.truncate(MAX_TAGS);
    tags
}

/// Whether `tag` is usable as a surrogate key.
pub fn valid_tag(tag: &str) -> bool {
    !tag.is_empty() && tag.len() <= MAX_TAG_LEN && tag.bytes().all(|b| b.is_ascii_graphic())
}

/// Combines the field lines of `name` and normalises whitespace around
/// commas, so `a,b` and `a, b` select the same variant.
fn normalized(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    let mut parts = Vec::new();
    let mut seen = false;
    for line in headers.get_all(name) {
        seen = true;
        let text = String::from_utf8_lossy(line.as_bytes());
        parts.extend(text.split(',').map(|p| p.trim().to_string()));
    }
    seen.then(|| parts.join(","))
}

/// Every stored variant of one URL, oldest first.
#[derive(Debug, Clone, Default)]
pub struct Entry {
    pub variants: Vec<Arc<CachedObject>>,
}

impl Entry {
    pub fn single(obj: Arc<CachedObject>) -> Self {
        Entry {
            variants: vec![obj],
        }
    }

    /// The newest variant matching the request.
    pub fn find(&self, req: &HeaderMap) -> Option<&Arc<CachedObject>> {
        self.variants.iter().rev().find(|v| v.matches(req))
    }

    /// A copy with `obj` added. A variant the request would have selected is
    /// replaced, and the oldest variants are dropped beyond `max_variants`,
    /// so an origin varying on something like User-Agent cannot grow one
    /// URL without bound.
    pub fn with_variant(
        &self,
        obj: Arc<CachedObject>,
        req: &HeaderMap,
        max_variants: usize,
    ) -> Entry {
        let mut variants: Vec<_> = self
            .variants
            .iter()
            .filter(|v| !v.matches(req))
            .cloned()
            .collect();
        variants.push(obj);
        let excess = variants.len().saturating_sub(max_variants.max(1));
        variants.drain(..excess);
        Entry { variants }
    }

    pub fn size(&self) -> usize {
        self.variants.iter().map(|v| v.size()).sum()
    }

    /// Surrogate keys of every variant, without duplicates.
    pub fn tags(&self) -> Vec<Arc<str>> {
        let mut tags: Vec<Arc<str>> = Vec::new();
        for tag in self.variants.iter().flat_map(|v| v.tags.iter()) {
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
        tags
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::{Duration, SystemTime};

    use http::HeaderValue;

    use super::*;

    pub(crate) fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    pub(crate) fn object(
        resp: &[(&str, &str)],
        req: &[(&str, &str)],
        body: &'static str,
    ) -> Arc<CachedObject> {
        let headers = map(resp);
        let freshness = Freshness {
            lifetime: Duration::from_secs(60),
            corrected_initial_age: Duration::ZERO,
            response_time: SystemTime::UNIX_EPOCH,
        };
        Arc::new(CachedObject::new(
            StatusCode::OK,
            headers,
            Bytes::from_static(body.as_bytes()),
            freshness,
            &map(req),
        ))
    }

    #[test]
    fn surrogate_keys_from_headers() {
        let h = map(&[
            ("surrogate-key", "product-1  home\tproduct-1"),
            ("surrogate-key", "news"),
        ]);
        let tags: Vec<_> = surrogate_keys(&h).iter().map(|t| t.to_string()).collect();
        assert_eq!(tags, ["product-1", "home", "news"]);
        let long = "x".repeat(MAX_TAG_LEN + 1);
        assert!(surrogate_keys(&map(&[("surrogate-key", &long)])).is_empty());
        let many: Vec<String> = (0..100).map(|i| format!("t{i}")).collect();
        assert_eq!(
            surrogate_keys(&map(&[("surrogate-key", &many.join(" "))])).len(),
            MAX_TAGS
        );
        let mut h = HeaderMap::new();
        h.insert(
            SURROGATE_KEY,
            http::HeaderValue::from_bytes(b"caf\xc3\xa9").unwrap(),
        );
        assert!(surrogate_keys(&h).is_empty());
        assert!(!valid_tag(""));
        assert!(!valid_tag("a b"));
    }

    #[test]
    fn primary_key_from_host_header() {
        let uri: Uri = "/a/b?x=1".parse().unwrap();
        let key = primary_key(&uri, &map(&[("host", "Example.COM:8080")])).unwrap();
        assert_eq!(&*key, "example.com:8080/a/b?x=1");
    }

    #[test]
    fn primary_key_from_absolute_form() {
        let uri: Uri = "http://Example.com/p".parse().unwrap();
        assert_eq!(
            &*primary_key(&uri, &HeaderMap::new()).unwrap(),
            "example.com/p"
        );
        let uri: Uri = "http://example.com".parse().unwrap();
        assert_eq!(
            &*primary_key(&uri, &HeaderMap::new()).unwrap(),
            "example.com/"
        );
    }

    #[test]
    fn primary_key_needs_a_host() {
        let uri: Uri = "/p".parse().unwrap();
        assert!(primary_key(&uri, &HeaderMap::new()).is_none());
        assert!(primary_key(&uri, &map(&[("host", " ")])).is_none());
    }

    #[test]
    fn object_without_vary_matches_everything() {
        let o = object(
            &[("cache-control", "max-age=60"), ("etag", "\"v\"")],
            &[],
            "x",
        );
        assert!(o.matches(&map(&[("accept-encoding", "gzip")])));
        assert!(o.vary().is_empty());
        assert_eq!(o.cc.max_age, Some(60));
        assert_eq!(o.etag.as_ref().unwrap().opaque, "v");
        assert!(o.size() > 1);
    }

    #[test]
    fn vary_selects_on_normalised_values() {
        let o = object(
            &[("vary", "Accept-Encoding, Accept-Language")],
            &[("accept-encoding", "gzip, br")],
            "x",
        );
        assert!(o.matches(&map(&[("accept-encoding", "gzip,br")])));
        assert!(o.matches(&map(&[
            ("accept-encoding", "gzip"),
            ("accept-encoding", "br")
        ])));
        assert!(!o.matches(&map(&[("accept-encoding", "br")])));
        // A header absent at store time must be absent at lookup time too.
        assert!(!o.matches(&map(&[
            ("accept-encoding", "gzip, br"),
            ("accept-language", "en")
        ])));
        assert!(!o.matches(&HeaderMap::new()));
    }

    #[test]
    fn entry_finds_newest_matching_variant() {
        let gz = object(
            &[("vary", "accept-encoding")],
            &[("accept-encoding", "gzip")],
            "gz",
        );
        let br = object(
            &[("vary", "accept-encoding")],
            &[("accept-encoding", "br")],
            "br",
        );
        let e = Entry::single(gz.clone()).with_variant(
            br.clone(),
            &map(&[("accept-encoding", "br")]),
            8,
        );
        assert_eq!(e.variants.len(), 2);
        assert_eq!(
            e.find(&map(&[("accept-encoding", "gzip")])).unwrap().body,
            "gz"
        );
        assert_eq!(
            e.find(&map(&[("accept-encoding", "br")])).unwrap().body,
            "br"
        );
        assert!(e.find(&map(&[("accept-encoding", "zstd")])).is_none());
        assert_eq!(e.size(), gz.size() + br.size());
        assert!(e.tags().is_empty());
        let a = object(
            &[("surrogate-key", "x y"), ("vary", "q")],
            &[("q", "0")],
            "a",
        );
        let b = object(
            &[("surrogate-key", "y z"), ("vary", "q")],
            &[("q", "1")],
            "b",
        );
        let e = Entry::single(a).with_variant(b, &map(&[("q", "1")]), 8);
        let tags: Vec<_> = e.tags().iter().map(|t| t.to_string()).collect();
        assert_eq!(tags, ["x", "y", "z"]);
    }

    #[test]
    fn entry_replaces_the_variant_the_request_selects() {
        let req = map(&[("accept-encoding", "gzip")]);
        let v1 = object(
            &[("vary", "accept-encoding")],
            &[("accept-encoding", "gzip")],
            "v1",
        );
        let v2 = object(
            &[("vary", "accept-encoding")],
            &[("accept-encoding", "gzip")],
            "v2",
        );
        let e = Entry::single(v1).with_variant(v2, &req, 8);
        assert_eq!(e.variants.len(), 1);
        assert_eq!(e.find(&req).unwrap().body, "v2");
    }

    #[test]
    fn entry_caps_variants() {
        let mut e = Entry::default();
        for ua in ["a", "b", "c", "d"] {
            let req = map(&[("user-agent", ua)]);
            let o = object(&[("vary", "user-agent")], &[("user-agent", ua)], "x");
            e = e.with_variant(o, &req, 3);
        }
        assert_eq!(e.variants.len(), 3);
        assert!(e.find(&map(&[("user-agent", "a")])).is_none());
        assert!(e.find(&map(&[("user-agent", "d")])).is_some());
        let e = e.with_variant(object(&[], &[], "y"), &HeaderMap::new(), 0);
        assert_eq!(e.variants.len(), 1);
    }

    #[test]
    fn non_utf8_selecting_values_compare_lossily() {
        let mut req = HeaderMap::new();
        req.insert("x-v", HeaderValue::from_bytes(b"\xffa").unwrap());
        let headers = map(&[("vary", "x-v")]);
        let f = Freshness {
            lifetime: Duration::ZERO,
            corrected_initial_age: Duration::ZERO,
            response_time: SystemTime::UNIX_EPOCH,
        };
        let o = CachedObject::new(StatusCode::OK, headers, Bytes::new(), f, &req);
        assert!(o.matches(&req));
    }
}
