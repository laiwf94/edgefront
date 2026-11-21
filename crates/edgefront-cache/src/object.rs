//! Stored responses, cache keys and Vary-aware variant selection
//! (RFC 9111 sections 2 and 4.1).

use std::sync::Arc;

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName};
use http::{StatusCode, Uri};

use crate::cache_control::CacheControl;
use crate::encoding::{self, Coding};
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
pub(crate) type VaryValues = Vec<(HeaderName, Option<String>)>;

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
    /// The body's content coding, `None` for one the cache does not
    /// negotiate.
    pub coding: Option<Coding>,
    /// Stored by a route that compresses ([`crate::variants`]): selected
    /// by Accept-Encoding through [`encoding::negotiate`] instead of the
    /// header's raw value, so a URL keeps at most one variant per coding.
    /// Routes that do not compress match Accept-Encoding raw, as cachet
    /// 0.1 did.
    pub normalised: bool,
    /// Whether the cache made this variant by compressing another one.
    pub derived: bool,
    vary: VaryValues,
}

/// The stored-only field marking a variant of a compressing route:
/// `source` for a response from upstream, the coding for one the cache
/// compressed. It lives in the stored headers, so the disk format carries
/// it, and [`crate::respond::from_cache`] never sends it.
pub const VARIANT: HeaderName = HeaderName::from_static("edgefront-variant");

/// [`VARIANT`]'s value on a response from upstream.
pub const SOURCE: &str = "source";

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
            coding: Coding::of_response(&headers),
            normalised: headers.contains_key(VARIANT),
            derived: headers.get(VARIANT).is_some_and(|v| v != SOURCE),
            cc: CacheControl::for_response(&headers),
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

    /// Whether this response was selected by the same header values as
    /// `req`. Accept-Encoding is left out for a variant of a known coding:
    /// whether the request accepts that coding is [`Entry::find`]'s choice.
    pub fn matches(&self, req: &HeaderMap) -> bool {
        self.vary.iter().all(|(name, value)| {
            (self.negotiated().is_some() && name == header::ACCEPT_ENCODING)
                || normalized(req, name) == *value
        })
    }

    /// The coding this variant is negotiated by, when it is.
    fn negotiated(&self) -> Option<Coding> {
        self.coding.filter(|_| self.normalised)
    }

    /// Whether `req` may be answered with this variant on its own, as a
    /// follower is: its Vary fields select it and it accepts the coding.
    pub fn selected_by(&self, req: &HeaderMap) -> bool {
        self.matches(req)
            && self
                .negotiated()
                .is_none_or(|c| encoding::acceptable(req, c))
    }

    /// Whether `self` and `other` answer the same requests apart from the
    /// coding: the same selecting values of every Vary field but
    /// Accept-Encoding.
    fn same_group(&self, other: &CachedObject) -> bool {
        let rest = |o: &CachedObject| {
            o.vary
                .iter()
                .filter(|(n, _)| n != header::ACCEPT_ENCODING)
                .cloned()
                .collect::<Vec<_>>()
        };
        rest(self) == rest(other)
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
pub(crate) const MAX_TAGS: usize = 64;
pub(crate) const MAX_TAG_LEN: usize = 128;

/// Reads surrogate keys from a response. Keys are visible ASCII without
/// spaces; overlong keys are dropped and at most `MAX_TAGS` are kept.
pub(crate) fn surrogate_keys(headers: &HeaderMap) -> Vec<Arc<str>> {
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
pub(crate) fn valid_tag(tag: &str) -> bool {
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

    /// The variant to answer the request with: among those its Vary
    /// fields select, the newest of the coding [`encoding::negotiate`]
    /// picks from the codings held, or else the newest of a coding the
    /// cache does not negotiate whose raw Accept-Encoding matched.
    pub fn find(&self, req: &HeaderMap) -> Option<&Arc<CachedObject>> {
        let mut codings = Vec::new();
        for v in self.variants.iter().filter(|v| v.matches(req)) {
            if let Some(c) = v.negotiated()
                && !codings.contains(&c)
            {
                codings.push(c);
            }
        }
        let chosen = encoding::negotiate(req, &codings);
        self.variants
            .iter()
            .rev()
            .filter(|v| v.matches(req))
            .find(|v| match v.negotiated() {
                Some(c) => Some(c) == chosen,
                None => true,
            })
    }

    /// The newest variant of the request's group in `coding`.
    pub fn find_coding(&self, req: &HeaderMap, coding: Coding) -> Option<&Arc<CachedObject>> {
        self.variants
            .iter()
            .rev()
            .find(|v| v.matches(req) && v.negotiated() == Some(coding))
    }

    /// A copy with `obj` added. A variant the request would have selected
    /// in the same coding is replaced; a response from upstream also drops
    /// the variants the cache compressed from the one it replaces. The
    /// oldest variants are dropped beyond `max_variants`, so an origin
    /// varying on something like User-Agent cannot grow one URL without
    /// bound; compressed variants do not count, and go with their source.
    pub(crate) fn with_variant(
        &self,
        obj: Arc<CachedObject>,
        req: &HeaderMap,
        max_variants: usize,
    ) -> Entry {
        let replaced = |v: &Arc<CachedObject>| match (v.negotiated(), obj.negotiated()) {
            (Some(_), Some(_)) => {
                v.same_group(&obj) && (v.coding == obj.coding || (v.derived && !obj.derived))
            }
            _ => v.matches(req),
        };
        let mut variants: Vec<_> = self
            .variants
            .iter()
            .filter(|v| !replaced(v))
            .cloned()
            .collect();
        variants.push(obj);
        let sources = variants.iter().filter(|v| !v.derived).count();
        let mut excess = sources.saturating_sub(max_variants.max(1));
        let mut kept = Vec::with_capacity(variants.len());
        for v in variants {
            if excess > 0 && !v.derived {
                excess -= 1;
                continue;
            }
            kept.push(v);
        }
        // A compressed variant whose source went goes too.
        let sources: Vec<_> = kept.iter().filter(|v| !v.derived).cloned().collect();
        kept.retain(|v| {
            !v.derived
                || sources
                    .iter()
                    .any(|s| s.coding == Some(Coding::Identity) && s.same_group(v))
        });
        Entry { variants: kept }
    }

    /// A copy with `derived`, compressed from `source`, added in place of
    /// any earlier ones of the same codings, or `None` when `source` is no
    /// longer held: a newer response replaced it while it was compressed.
    pub(crate) fn with_derived(
        &self,
        source: &Arc<CachedObject>,
        derived: &[Arc<CachedObject>],
    ) -> Option<Entry> {
        if !self.variants.iter().any(|v| Arc::ptr_eq(v, source)) {
            return None;
        }
        let mut variants: Vec<_> = self
            .variants
            .iter()
            .filter(|v| {
                !(v.derived && v.same_group(source) && derived.iter().any(|d| d.coding == v.coding))
            })
            .cloned()
            .collect();
        variants.extend(derived.iter().cloned());
        Some(Entry { variants })
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

    /// A response from upstream stored by a compressing route.
    fn upstream(
        resp: &[(&str, &str)],
        req: &[(&str, &str)],
        body: &'static str,
    ) -> Arc<CachedObject> {
        let mut resp = resp.to_vec();
        resp.push(("edgefront-variant", "source"));
        object(&resp, req, body)
    }

    /// A variant the cache compressed from `source` in `coding`.
    fn derived(source: &Arc<CachedObject>, coding: &str) -> Arc<CachedObject> {
        let mut headers = source.headers.clone();
        headers.insert(
            header::CONTENT_ENCODING,
            HeaderValue::from_str(coding).unwrap(),
        );
        headers.insert(VARIANT, HeaderValue::from_str(coding).unwrap());
        Arc::new(CachedObject::with_vary(
            source.status,
            headers,
            Bytes::from_static(b"z"),
            source.freshness,
            source.vary().clone(),
        ))
    }

    fn coding_of(e: &Entry, ae: &str) -> Option<Option<Coding>> {
        let req = if ae == "-" {
            map(&[])
        } else {
            map(&[("accept-encoding", ae)])
        };
        e.find(&req).map(|v| v.coding)
    }

    #[test]
    fn variants_are_chosen_by_normalised_accept_encoding() {
        let vary = [("vary", "accept-encoding")];
        let source = upstream(&vary, &[("accept-encoding", "gzip, deflate, br")], "plain");
        assert_eq!(source.coding, Some(Coding::Identity));
        assert!(source.normalised && !source.derived);
        let mut e = Entry::single(source.clone());
        e = e
            .with_derived(&source, &[derived(&source, "br"), derived(&source, "gzip")])
            .unwrap();
        assert_eq!(e.variants.len(), 3);
        // However the header is spelled, one of the held codings answers.
        assert_eq!(coding_of(&e, "gzip, deflate, br"), Some(Some(Coding::Br)));
        assert_eq!(
            coding_of(&e, "br;q=1.0, gzip;q=0.5"),
            Some(Some(Coding::Br))
        );
        assert_eq!(coding_of(&e, "gzip"), Some(Some(Coding::Gzip)));
        assert_eq!(coding_of(&e, "zstd"), Some(Some(Coding::Identity)));
        assert_eq!(coding_of(&e, "-"), Some(Some(Coding::Identity)));
        assert_eq!(coding_of(&e, "*;q=0"), None);
        assert_eq!(
            coding_of(&e, "identity;q=0, gzip"),
            Some(Some(Coding::Gzip))
        );
        // A follower is selected the same way, coding included.
        let br = e.find(&map(&[("accept-encoding", "br")])).unwrap();
        assert!(br.selected_by(&map(&[("accept-encoding", "br")])));
        assert!(!br.selected_by(&map(&[("accept-encoding", "gzip")])));
        assert!(br.selected_by(&map(&[])));
    }

    #[test]
    fn derived_variants_follow_their_source() {
        let vary = [("vary", "accept-encoding")];
        let source = upstream(&vary, &[("accept-encoding", "br")], "v1");
        let e = Entry::single(source.clone())
            .with_derived(&source, &[derived(&source, "br"), derived(&source, "zstd")])
            .unwrap();
        // Compressed variants do not count against max_variants.
        assert_eq!(e.variants.len(), 3);
        // Compressing again replaces the earlier variant of that coding.
        let again = e.with_derived(&source, &[derived(&source, "br")]).unwrap();
        assert_eq!(again.variants.len(), 3);
        // A newer response from upstream drops what came from the old one,
        // whatever Accept-Encoding the request that fetched it sent.
        let newer = upstream(&vary, &[("accept-encoding", "gzip")], "v2");
        let replaced = e.with_variant(newer.clone(), &map(&[("accept-encoding", "gzip")]), 1);
        assert_eq!(replaced.variants.len(), 1);
        assert!(Arc::ptr_eq(&replaced.variants[0], &newer));
        // Variants compressed from a source no longer held are refused.
        assert!(
            replaced
                .with_derived(&source, &[derived(&source, "br")])
                .is_none()
        );
        // An origin response in a coding replaces only that coding.
        let gz = map(&[
            ("vary", "accept-encoding"),
            ("content-encoding", "gzip"),
            ("edgefront-variant", "source"),
        ]);
        let origin_gzip = Arc::new(CachedObject::new(
            StatusCode::OK,
            gz,
            Bytes::from_static(b"g"),
            newer.freshness,
            &map(&[("accept-encoding", "gzip")]),
        ));
        let both = replaced.with_variant(origin_gzip, &map(&[("accept-encoding", "gzip")]), 8);
        assert_eq!(both.variants.len(), 2);
        assert_eq!(coding_of(&both, "gzip"), Some(Some(Coding::Gzip)));
        assert_eq!(coding_of(&both, "br"), Some(Some(Coding::Identity)));
    }

    #[test]
    fn evicting_a_source_takes_its_compressed_variants() {
        let vary = [("vary", "accept-encoding, accept-language")];
        let en = upstream(&vary, &[("accept-language", "en")], "en");
        let e = Entry::single(en.clone())
            .with_derived(&en, &[derived(&en, "br")])
            .unwrap();
        let fr = upstream(&vary, &[("accept-language", "fr")], "fr");
        // One source allowed: the French one pushes out English and its br.
        let e = e.with_variant(fr.clone(), &map(&[("accept-language", "fr")]), 1);
        assert_eq!(e.variants.len(), 1);
        assert!(Arc::ptr_eq(&e.variants[0], &fr));
        // Another language's request does not see English variants.
        let e = Entry::single(en.clone())
            .with_derived(&en, &[derived(&en, "br")])
            .unwrap()
            .with_variant(fr, &map(&[("accept-language", "fr")]), 8);
        let fr_br = map(&[("accept-language", "fr"), ("accept-encoding", "br")]);
        assert_eq!(e.find(&fr_br).unwrap().body, Bytes::from_static(b"fr"));
        let en_br = map(&[("accept-language", "en"), ("accept-encoding", "br")]);
        assert!(e.find(&en_br).unwrap().derived);
    }

    #[test]
    fn routes_that_do_not_compress_match_accept_encoding_raw() {
        let plain = object(
            &[("vary", "accept-encoding")],
            &[("accept-encoding", "gzip")],
            "g",
        );
        assert!(!plain.normalised);
        let e = Entry::single(plain);
        assert!(e.find(&map(&[("accept-encoding", "gzip")])).is_some());
        assert!(e.find(&map(&[("accept-encoding", "br, gzip")])).is_none());
    }

    #[test]
    fn unknown_codings_keep_raw_matching() {
        let deflate = object(
            &[("vary", "accept-encoding"), ("content-encoding", "deflate")],
            &[("accept-encoding", "deflate")],
            "d",
        );
        assert_eq!(deflate.coding, None);
        let e = Entry::single(deflate);
        assert!(e.find(&map(&[("accept-encoding", "deflate")])).is_some());
        assert!(e.find(&map(&[("accept-encoding", "gzip")])).is_none());
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
