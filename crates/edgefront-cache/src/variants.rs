//! Compressed variants of stored responses.
//!
//! When a route compresses, a stored `200` whose body is text-like, at
//! least [`CompressionSettings::min_bytes`] long, sent without a content
//! coding and without `no-transform` is compressed once per coding (br,
//! zstd and gzip) after it is stored, on tokio's blocking pool so a fill
//! never waits for it. Each coding is stored as its own variant under the
//! same primary key, next to the identity source, so one URL holds at most
//! four of them ([`crate::encoding`] maps every request to one), they are
//! evicted and purged together as one entry, and a newer response from
//! upstream drops the ones made from the copy it replaces. A coding whose
//! output is not smaller than the source is not stored.
//!
//! The source gets `Vary: Accept-Encoding` before it is stored, so every
//! answer about the URL says it depends on the header. A compressed
//! variant is a different representation (RFC 9110 section 8.8.3), so its
//! entity tag is made weak: `"v1"` becomes `W/"v1"`, which still validates
//! with the origin's weak comparison but never satisfies a strong one.
//!
//! A compressed variant is never revalidated on its own: when it goes
//! stale, the layer revalidates its source, and compresses again.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use bytes::Bytes;
use http::StatusCode;
use http::header::{self, HeaderMap, HeaderValue};
use tokio::sync::Notify;

use crate::cache_control::CacheControl;
use crate::encoding::{Coding, CompressionStats};
use crate::headers::field_list;
use crate::object::{CachedObject, Key, SOURCE, VARIANT};
use crate::slice::Layout;
use crate::store::Cache;

/// The codings the cache compresses into, best first.
pub const CODINGS: [Coding; 3] = [Coding::Br, Coding::Zstd, Coding::Gzip];

/// How a route's responses are compressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressionSettings {
    /// Bodies shorter than this are stored as they are: the coding's own
    /// framing would eat most of the gain.
    pub min_bytes: usize,
    /// Brotli quality, 0 to 11.
    pub brotli_quality: u32,
    /// zstd level, 1 to 19.
    pub zstd_level: i32,
    /// gzip level, 1 to 9.
    pub gzip_level: u32,
    /// The CPU budget: compression jobs queued or running on the blocking
    /// pool at once. Past it, compression is skipped and counted.
    pub queue_limit: usize,
}

impl Default for CompressionSettings {
    /// Levels a CDN uses for compression on fill: well past the curve's
    /// knee for ratio, still a few milliseconds per hundred KiB.
    fn default() -> Self {
        CompressionSettings {
            min_bytes: 256,
            brotli_quality: 5,
            zstd_level: 3,
            gzip_level: 6,
            queue_limit: 64,
        }
    }
}

/// Whether a media type is text-like, so worth compressing: `text/*`,
/// JSON, XML, JavaScript, SVG, WebAssembly, and the uncompressed font
/// formats.
pub fn compressible_type(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let Some((top, sub)) = essence.split_once('/') else {
        return false;
    };
    top == "text"
        || sub.ends_with("+json")
        || sub.ends_with("+xml")
        || matches!(
            essence.as_str(),
            "application/javascript"
                | "application/x-javascript"
                | "application/ecmascript"
                | "application/json"
                | "application/xml"
                | "application/wasm"
                | "image/svg+xml"
                | "image/x-icon"
                | "font/ttf"
                | "font/otf"
                | "application/vnd.ms-fontobject"
        )
}

/// Whether a response with these headers may be compressed by the cache:
/// a text-like type, no content coding, no `no-transform`, not a range.
pub fn compressible_headers(status: StatusCode, headers: &HeaderMap) -> bool {
    status == StatusCode::OK
        && !headers.contains_key(header::CONTENT_ENCODING)
        && !headers.contains_key(header::CONTENT_RANGE)
        && !CacheControl::for_response(headers).no_transform
        && headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(compressible_type)
}

/// Whether the stored `obj` is a source the cache compresses.
pub fn compressible(obj: &CachedObject, settings: &CompressionSettings) -> bool {
    obj.normalised
        && !obj.derived
        && obj.coding == Some(Coding::Identity)
        && obj.body.len() >= settings.min_bytes.max(1)
        && Layout::of(obj).is_none()
        && compressible_headers(obj.status, &obj.headers)
}

/// Marks a response about to be stored as a compression source: the
/// [`VARIANT`] marker, and `Accept-Encoding` in its Vary unless it is
/// already there or Vary is `*`.
pub fn vary_on_encoding(headers: &mut HeaderMap) {
    headers.insert(VARIANT, HeaderValue::from_static(SOURCE));
    let listed = field_list(headers, &header::VARY);
    if listed
        .iter()
        .any(|v| v == "*" || v.eq_ignore_ascii_case("accept-encoding"))
    {
        return;
    }
    headers.append(header::VARY, HeaderValue::from_static("accept-encoding"));
}

/// Compresses `data` in `coding`. `None` for identity, or without the
/// `compression` feature.
pub fn compress(coding: Coding, data: &[u8], settings: &CompressionSettings) -> Option<Vec<u8>> {
    #[cfg(feature = "compression")]
    {
        codec::compress(coding, data, settings).ok()
    }
    #[cfg(not(feature = "compression"))]
    {
        let _ = (coding, data, settings);
        None
    }
}

/// Decompresses a body in `coding`, reading at most `limit` bytes of
/// output. For tests, the demo and the dictionary module.
#[cfg(feature = "compression")]
pub fn decompress(coding: Coding, data: &[u8], limit: usize) -> std::io::Result<Vec<u8>> {
    codec::decompress(coding, data, limit)
}

/// The weak form of an entity tag field value: `"x"` to `W/"x"`, a weak
/// tag unchanged.
pub fn weakened(etag: &HeaderValue) -> HeaderValue {
    let bytes = etag.as_bytes();
    if bytes.starts_with(b"W/") {
        return etag.clone();
    }
    let mut out = Vec::with_capacity(bytes.len() + 2);
    out.extend_from_slice(b"W/");
    out.extend_from_slice(bytes);
    HeaderValue::from_bytes(&out).unwrap_or_else(|_| etag.clone())
}

/// The variant of `source` with `body`, compressed in `coding`.
pub fn derived_variant(source: &CachedObject, coding: Coding, body: Vec<u8>) -> CachedObject {
    let mut headers = source.headers.clone();
    headers.insert(
        header::CONTENT_ENCODING,
        HeaderValue::from_static(coding.as_str()),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    if let Some(etag) = source.headers.get(header::ETAG) {
        headers.insert(header::ETAG, weakened(etag));
    }
    headers.insert(VARIANT, HeaderValue::from_static(coding.as_str()));
    CachedObject::with_vary(
        source.status,
        headers,
        Bytes::from(body),
        source.freshness,
        source.vary().clone(),
    )
}

/// Compresses `source` into every coding of [`CODINGS`] that makes it
/// smaller, recording each in `stats`. Blocking: run it on the blocking
/// pool.
pub fn derive(
    source: &CachedObject,
    settings: &CompressionSettings,
    stats: &CompressionStats,
) -> Vec<Arc<CachedObject>> {
    CODINGS
        .into_iter()
        .filter_map(|coding| {
            let started = Instant::now();
            let body = compress(coding, &source.body, settings)?;
            stats.record(
                coding.as_str(),
                source.body.len(),
                body.len(),
                started.elapsed(),
            );
            (body.len() < source.body.len())
                .then(|| Arc::new(derived_variant(source, coding, body)))
        })
        .collect()
}

/// Runs compression on tokio's blocking pool within a budget: at most
/// [`CompressionSettings::queue_limit`] jobs queued or running, beyond
/// which compression on fill and on the fly is skipped (and counted), so a
/// burst of fills cannot queue unbounded CPU work.
#[derive(Debug, Default)]
pub struct Compressor {
    settings: CompressionSettings,
    in_flight: AtomicUsize,
    idle: Notify,
    stats: CompressionStats,
}

/// A place in the compressor's budget, given back when dropped.
#[derive(Debug)]
pub struct Slot<'a>(&'a Compressor);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

impl Compressor {
    pub fn new(settings: CompressionSettings) -> Self {
        Compressor {
            settings,
            in_flight: AtomicUsize::new(0),
            idle: Notify::new(),
            stats: CompressionStats::default(),
        }
    }

    pub fn settings(&self) -> &CompressionSettings {
        &self.settings
    }

    pub fn stats(&self) -> &CompressionStats {
        &self.stats
    }

    /// Jobs queued or running.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    /// Takes a place in the budget, or counts a skip when the queue is at
    /// its limit.
    fn admit(&self) -> bool {
        let limit = self.settings.queue_limit.max(1);
        let mut n = self.in_flight.load(Ordering::Acquire);
        let admitted = loop {
            if n >= limit {
                break false;
            }
            match self.in_flight.compare_exchange_weak(
                n,
                n + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break true,
                Err(now) => n = now,
            }
        };
        if !admitted {
            self.stats.skip();
        }
        admitted
    }

    fn release(&self) {
        if self.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.idle.notify_waiters();
        }
    }

    /// A place for one on-the-fly compression the caller runs itself, or
    /// `None` over the budget.
    pub fn slot(&self) -> Option<Slot<'_>> {
        // Lazily: a Slot made and dropped would give back a place never taken.
        self.admit().then(|| Slot(self))
    }

    /// Compresses the stored `source` of `key` on the blocking pool and
    /// adds the variants to its entry, unless a newer response replaced
    /// it meanwhile or a purge since `epoch` removed it. Does nothing for
    /// a response that is not a compression source, and skips it when the
    /// queue is at its limit: the source is still served, uncompressed.
    pub fn submit(
        self: &Arc<Self>,
        cache: Arc<Cache>,
        key: Key,
        source: Arc<CachedObject>,
        epoch: u64,
    ) {
        if !compressible(&source, &self.settings) || !cfg!(feature = "compression") || !self.admit()
        {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let job = this.clone();
            let job_source = source.clone();
            let derived =
                tokio::task::spawn_blocking(move || derive(&job_source, &job.settings, &job.stats))
                    .await;
            if let Ok(derived) = derived
                && !derived.is_empty()
            {
                cache.store_derived(key, &source, &derived, epoch);
            }
            this.release();
        });
    }

    /// Waits until no job is queued or running.
    pub async fn settled(&self) {
        loop {
            let idle = self.idle.notified();
            if self.in_flight() == 0 {
                return;
            }
            idle.await;
        }
    }
}

#[cfg(feature = "compression")]
mod codec {
    use std::io::{self, Read, Write};

    use super::{Coding, CompressionSettings};

    pub fn compress(coding: Coding, data: &[u8], s: &CompressionSettings) -> io::Result<Vec<u8>> {
        match coding {
            Coding::Identity => Err(io::Error::other("identity is not a compression")),
            Coding::Gzip => {
                let mut enc = flate2::write::GzEncoder::new(
                    Vec::with_capacity(data.len() / 3),
                    flate2::Compression::new(s.gzip_level.clamp(1, 9)),
                );
                enc.write_all(data)?;
                enc.finish()
            }
            Coding::Br => {
                let mut out = Vec::with_capacity(data.len() / 4);
                let params = brotli::enc::BrotliEncoderParams {
                    quality: s.brotli_quality.min(11) as i32,
                    lgwin: 22,
                    size_hint: data.len(),
                    ..Default::default()
                };
                brotli::BrotliCompress(&mut &data[..], &mut out, &params)?;
                Ok(out)
            }
            Coding::Zstd => zstd::bulk::compress(data, s.zstd_level.clamp(1, 19)),
        }
    }

    pub fn decompress(coding: Coding, data: &[u8], limit: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let cap = limit as u64 + 1;
        match coding {
            Coding::Identity => out.extend_from_slice(data),
            Coding::Gzip => {
                flate2::read::GzDecoder::new(data)
                    .take(cap)
                    .read_to_end(&mut out)?;
            }
            Coding::Br => {
                brotli::Decompressor::new(data, 4096)
                    .take(cap)
                    .read_to_end(&mut out)?;
            }
            Coding::Zstd => {
                zstd::stream::read::Decoder::new(data)?
                    .take(cap)
                    .read_to_end(&mut out)?;
            }
        }
        if out.len() > limit {
            return Err(io::Error::other("decompressed body over the limit"));
        }
        Ok(out)
    }
}

#[cfg(all(test, feature = "compression"))]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::object::tests::map;
    use crate::policy::Freshness;

    fn text(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut i = 0u32;
        while out.len() < len {
            out.extend_from_slice(
                format!("function f{}(a) {{ return a + {}; }}\n", i % 97, i).as_bytes(),
            );
            i += 1;
        }
        out.truncate(len);
        out
    }

    fn source(resp: &[(&str, &str)], body: Vec<u8>) -> CachedObject {
        let mut headers = map(resp);
        vary_on_encoding(&mut headers);
        CachedObject::new(
            StatusCode::OK,
            headers,
            Bytes::from(body),
            Freshness {
                lifetime: Duration::from_secs(60),
                corrected_initial_age: Duration::ZERO,
                response_time: SystemTime::UNIX_EPOCH,
            },
            &map(&[("accept-encoding", "br")]),
        )
    }

    #[test]
    fn only_text_like_identity_responses_are_compressed() {
        for t in [
            "text/html; charset=utf-8",
            "Text/CSS",
            "application/javascript",
            "application/json",
            "application/ld+json",
            "application/atom+xml",
            "image/svg+xml",
            "application/wasm",
        ] {
            assert!(compressible_type(t), "{t}");
        }
        for t in [
            "image/png",
            "video/mp4",
            "application/octet-stream",
            "font/woff2",
            "",
            "text",
        ] {
            assert!(!compressible_type(t), "{t}");
        }
        let s = CompressionSettings::default();
        let js = [("content-type", "application/javascript")];
        assert!(compressible(&source(&js, text(1000)), &s));
        assert!(
            !compressible(&source(&js, text(100)), &s),
            "under min_bytes"
        );
        let mut unmarked = source(&js, text(1000));
        unmarked.normalised = false;
        assert!(
            !compressible(&unmarked, &s),
            "not stored by a compressing route"
        );
        let png = [("content-type", "image/png")];
        assert!(!compressible(&source(&png, text(1000)), &s));
        let coded = [("content-type", "text/html"), ("content-encoding", "gzip")];
        assert!(!compressible(&source(&coded, text(1000)), &s));
        let nt = [
            ("content-type", "text/html"),
            ("cache-control", "max-age=60, no-transform"),
        ];
        assert!(!compressible(&source(&nt, text(1000)), &s));
        let ranged = [
            ("content-type", "text/html"),
            ("content-range", "bytes 0-9/100"),
        ];
        assert!(!compressible(&source(&ranged, text(1000)), &s));
    }

    #[test]
    fn vary_gets_accept_encoding_once() {
        let mut h = map(&[("vary", "Accept-Language")]);
        vary_on_encoding(&mut h);
        vary_on_encoding(&mut h);
        assert_eq!(
            field_list(&h, &header::VARY),
            ["accept-language", "accept-encoding"]
        );
        let mut star = map(&[("vary", "*")]);
        vary_on_encoding(&mut star);
        assert_eq!(field_list(&star, &header::VARY), ["*"]);
        let mut already = map(&[("vary", "accept-encoding")]);
        vary_on_encoding(&mut already);
        assert_eq!(already.get_all(header::VARY).iter().count(), 1);
    }

    #[test]
    fn derived_variants_round_trip_and_mark_themselves() {
        let body = text(64 << 10);
        let src = source(
            &[("content-type", "text/javascript"), ("etag", "\"v1\"")],
            body.clone(),
        );
        let out = derive(
            &src,
            &CompressionSettings::default(),
            &CompressionStats::default(),
        );
        let codings: Vec<_> = out.iter().map(|v| v.coding).collect();
        assert_eq!(
            codings,
            [Some(Coding::Br), Some(Coding::Zstd), Some(Coding::Gzip)]
        );
        for v in &out {
            let c = v.coding.unwrap();
            assert!(v.derived);
            assert!(
                v.body.len() * 4 < body.len(),
                "{c:?} {} bytes",
                v.body.len()
            );
            assert_eq!(decompress(c, &v.body, body.len()).unwrap(), body);
            assert_eq!(v.headers[header::CONTENT_ENCODING], c.as_str());
            assert_eq!(
                v.headers[header::CONTENT_LENGTH],
                v.body.len().to_string().as_str()
            );
            assert_eq!(v.headers[header::ETAG], "W/\"v1\"");
            assert_eq!(v.vary(), src.vary());
            assert!(!compressible(v, &CompressionSettings::default()));
        }
        // A weak tag stays as it is.
        let weak = source(
            &[("content-type", "text/css"), ("etag", "W/\"w\"")],
            text(4096),
        );
        assert_eq!(
            derive(
                &weak,
                &CompressionSettings::default(),
                &CompressionStats::default()
            )[0]
            .headers[header::ETAG],
            "W/\"w\""
        );
    }

    #[test]
    fn incompressible_codings_are_not_stored_and_limits_hold() {
        // Random bytes do not shrink, so no coding is kept.
        let mut x = 0x9e37_79b9_u32;
        let noise: Vec<u8> = (0..4096)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        let src = source(&[("content-type", "text/plain")], noise);
        assert!(
            derive(
                &src,
                &CompressionSettings::default(),
                &CompressionStats::default()
            )
            .is_empty()
        );
        assert!(compress(Coding::Identity, b"x", &CompressionSettings::default()).is_none());
        // Decompression stops at the limit.
        let big = compress(Coding::Gzip, &text(10_000), &CompressionSettings::default()).unwrap();
        assert!(decompress(Coding::Gzip, &big, 9_999).is_err());
        assert_eq!(
            decompress(Coding::Gzip, &big, 10_000).unwrap().len(),
            10_000
        );
        assert!(decompress(Coding::Br, b"not brotli", 100).is_err());
        assert!(decompress(Coding::Zstd, b"not zstd", 100).is_err());
        assert_eq!(decompress(Coding::Identity, b"abc", 3).unwrap(), b"abc");
    }

    #[tokio::test]
    async fn compression_over_the_queue_limit_is_skipped_and_counted() {
        let c = Arc::new(Compressor::new(CompressionSettings {
            queue_limit: 1,
            ..CompressionSettings::default()
        }));
        let cache =
            Arc::new(crate::store::Cache::new(&crate::store::CacheSettings::default()).unwrap());
        let src = Arc::new(source(&[("content-type", "text/css")], text(8192)));
        let key: Key = "a.example/x.css".into();
        assert!(cache.store(key.clone(), src.clone(), &map(&[]), cache.epoch()));
        // The one place is taken: the fill's compression is skipped.
        let held = c.slot().unwrap();
        assert!(c.slot().is_none());
        c.submit(cache.clone(), key.clone(), src.clone(), cache.epoch());
        assert_eq!((c.in_flight(), c.stats().skipped()), (1, 2));
        drop(held);
        assert_eq!(c.in_flight(), 0);
        // Free again: compressed, with every coding counted.
        c.submit(cache.clone(), key.clone(), src.clone(), cache.epoch());
        c.settled().await;
        assert_eq!(cache.memory().get(&key).unwrap().variants.len(), 4);
        for coding in ["br", "zstd", "gzip"] {
            let (bytes_in, bytes_out) = c.stats().bytes(coding);
            assert_eq!(bytes_in, 8192, "{coding}");
            assert!(bytes_out > 0 && bytes_out < 8192, "{coding}");
            assert!(c.stats().micros_per_mib(coding).unwrap() > 0.0);
        }
        assert_eq!(c.stats().skipped(), 2);
    }
}
