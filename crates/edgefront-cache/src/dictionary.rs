//! Compression Dictionary Transport (RFC 9842): shared dictionaries that
//! make a repeat download of a changed resource tiny.
//!
//! - **Capture.** A stored identity response with `Use-As-Dictionary`
//!   becomes a dictionary: its body, the SHA-256 of that body, the URL
//!   pattern it applies to and the destinations (`match-dest`) it is
//!   for. It is kept while the response is fresh, never longer than
//!   [`DictionarySettings::max_ttl`], within a byte budget.
//! - **Matching.** A request that names a dictionary by its hash in
//!   `Available-Dictionary`, accepts `dcb` or `dcz` in Accept-Encoding,
//!   is for the dictionary's host, matches its pattern and, when the
//!   dictionary lists destinations, has a `Sec-Fetch-Dest` among them,
//!   is answered from the stored copy compressed with that dictionary.
//! - **Encodings.** `dcb` is brotli with the dictionary as a raw prefix,
//!   `dcz` zstd with it as raw content; both start with their magic and
//!   the dictionary's hash, so a client can check it used the right one.
//!   The response carries `Vary: accept-encoding, available-dictionary`.
//! - **Storage.** Compressed bodies are kept per URL, dictionary and
//!   coding next to the dictionaries, tied to the stored copy they came
//!   from, so a changed resource is compressed again and the budget
//!   holds.
//!
//! Patterns are a subset of the URL Pattern standard that covers what
//! sites send: a path, absolute or relative to the dictionary's URL, or
//! a same-origin absolute URL, with `*` for any run of characters and
//! `\` to escape. Patterns with groups, named parameters or regular
//! expressions, and dictionary types other than `raw`, are not used.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use http::header::{HeaderMap, HeaderName};
use sha2::{Digest, Sha256};

use crate::object::{CachedObject, Key};

/// The response field that offers a response as a dictionary.
pub const USE_AS_DICTIONARY: HeaderName = HeaderName::from_static("use-as-dictionary");
/// The request field naming the dictionary the client holds, by hash.
pub const AVAILABLE_DICTIONARY: HeaderName = HeaderName::from_static("available-dictionary");
/// The request field echoing the dictionary's `id`.
pub const DICTIONARY_ID: HeaderName = HeaderName::from_static("dictionary-id");

/// A dictionary's SHA-256.
pub type Hash = [u8; 32];

/// The dictionary-compressed content codings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DictCoding {
    /// Dictionary-compressed brotli.
    Dcb,
    /// Dictionary-compressed zstd.
    Dcz,
}

impl DictCoding {
    pub fn as_str(self) -> &'static str {
        match self {
            DictCoding::Dcb => "dcb",
            DictCoding::Dcz => "dcz",
        }
    }

    /// The bytes before the compressed stream: the coding's magic, then
    /// the dictionary's hash (RFC 9842 sections 4.1 and 4.2).
    fn magic(self) -> &'static [u8] {
        match self {
            DictCoding::Dcb => &[0xff, 0x44, 0x43, 0x42],
            DictCoding::Dcz => &[0x5e, 0x2a, 0x4d, 0x18, 0x20, 0x00, 0x00, 0x00],
        }
    }

    /// The header length of a body in this coding.
    pub fn header_len(self) -> usize {
        self.magic().len() + 32
    }
}

/// The SHA-256 of a dictionary's bytes.
pub fn hash(bytes: &[u8]) -> Hash {
    Sha256::digest(bytes).into()
}

/// A parsed `Use-As-Dictionary`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseAsDictionary {
    pub pattern: String,
    pub dest: Vec<String>,
    pub id: Option<String>,
}

/// The longest `id` RFC 9842 allows.
const MAX_ID: usize = 1024;
/// The longest pattern used.
const MAX_PATTERN: usize = 2048;

/// Parses an sf-string at the start of `s`: the string and the rest.
fn sf_string(s: &str) -> Option<(String, &str)> {
    let rest = s.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((out, &rest[i + 1..])),
            '\\' => match chars.next() {
                Some((_, e @ ('"' | '\\'))) => out.push(e),
                _ => return None,
            },
            ' '..='~' => out.push(c),
            _ => return None,
        }
    }
    None
}

/// Parses `Use-As-Dictionary`, a structured-field dictionary (RFC 8941)
/// with `match` (required), `match-dest`, `id` and `type`. Unknown keys
/// are ignored, as the RFC asks; a value of the wrong type, a missing
/// `match`, or a type other than `raw` gives `None`.
pub fn parse_use_as_dictionary(value: &str) -> Option<UseAsDictionary> {
    let mut pattern = None;
    let mut dest = Vec::new();
    let mut id = None;
    let mut rest = value.trim();
    while !rest.is_empty() {
        let key_end = rest
            .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || "_-.*".contains(c)))
            .unwrap_or(rest.len());
        let (key, after) = rest.split_at(key_end);
        if key.is_empty() {
            return None;
        }
        let after = after.strip_prefix('=')?;
        let tail = match key {
            "match" => {
                let (s, tail) = sf_string(after)?;
                pattern = Some(s);
                tail
            }
            "id" => {
                let (s, tail) = sf_string(after)?;
                if s.len() > MAX_ID {
                    return None;
                }
                id = Some(s);
                tail
            }
            "match-dest" => {
                let mut list = after.strip_prefix('(')?;
                loop {
                    list = list.trim_start_matches(' ');
                    if let Some(tail) = list.strip_prefix(')') {
                        break tail;
                    }
                    let (s, tail) = sf_string(list)?;
                    dest.push(s);
                    list = tail;
                }
            }
            "type" => {
                let end = after.find([';', ',', ' ']).unwrap_or(after.len());
                if &after[..end] != "raw" {
                    return None;
                }
                &after[end..]
            }
            _ => {
                // An unknown member: skip its value.
                let end = if after.starts_with('"') {
                    let (_, tail) = sf_string(after)?;
                    after.len() - tail.len()
                } else {
                    after.find([',']).unwrap_or(after.len())
                };
                &after[end..]
            }
        };
        let tail = tail.trim_start_matches(' ');
        rest = match tail.strip_prefix(',') {
            Some(t) => t.trim_start_matches(' '),
            None if tail.is_empty() => tail,
            None => return None,
        };
    }
    let pattern = pattern.filter(|p| !p.is_empty() && p.len() <= MAX_PATTERN)?;
    Some(UseAsDictionary { pattern, dest, id })
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding, as structured-field byte sequences use.
pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unbase64(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    if !s.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for chunk in s.chunks(4) {
        let mut n = 0u32;
        let mut pad = 0;
        for (i, &c) in chunk.iter().enumerate() {
            let v = if c == b'=' && i >= 2 {
                pad += 1;
                0
            } else if pad > 0 {
                return None;
            } else {
                B64.iter().position(|&b| b == c)? as u32
            };
            n = (n << 6) | v;
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Some(out)
}

/// The hash in `Available-Dictionary`: one structured-field byte
/// sequence, `:base64:`, of exactly 32 bytes.
pub fn available(headers: &HeaderMap) -> Option<Hash> {
    let mut lines = headers.get_all(AVAILABLE_DICTIONARY).iter();
    let line = lines.next()?;
    if lines.next().is_some() {
        return None;
    }
    let text = line.to_str().ok()?.trim();
    let inner = text.strip_prefix(':')?.strip_suffix(':')?;
    unbase64(inner)?.try_into().ok()
}

/// The dictionary codings the request accepts, preferring dcb on a tie.
pub fn accepted(headers: &HeaderMap) -> Option<DictCoding> {
    let mut best: Option<(u16, DictCoding)> = None;
    for member in crate::headers::field_list(headers, &http::header::ACCEPT_ENCODING) {
        let mut parts = member.split(';');
        let token = parts.next().unwrap_or("").trim();
        let coding = if token.eq_ignore_ascii_case("dcb") {
            DictCoding::Dcb
        } else if token.eq_ignore_ascii_case("dcz") {
            DictCoding::Dcz
        } else {
            continue;
        };
        let mut q = Some(1000);
        for p in parts {
            match p.split_once('=') {
                Some((k, v)) if k.trim().eq_ignore_ascii_case("q") => {
                    q = crate::encoding::parse_q(v.trim());
                }
                Some(_) => {}
                None => q = None,
            }
        }
        if let Some(q) = q.filter(|q| *q > 0)
            && best
                .is_none_or(|(b, c)| q > b || (q == b && coding == DictCoding::Dcb && c != coding))
        {
            best = Some((q, coding));
        }
    }
    best.map(|(_, c)| c)
}

/// Resolves a pattern against the dictionary's path: a path pattern stays,
/// a relative one joins the dictionary's directory, and an absolute URL
/// must name `host`. `None` for a pattern this module does not use.
fn resolve(pattern: &str, host: &str, dict_path: &str) -> Option<String> {
    if pattern.contains(['(', ')', '{', '}', '?', '#', '+']) || has_named_group(pattern) {
        return None;
    }
    if let Some(rest) = pattern
        .strip_prefix("https://")
        .or_else(|| pattern.strip_prefix("http://"))
    {
        let (authority, path) = rest.split_at(rest.find('/')?);
        if !authority.eq_ignore_ascii_case(host) {
            return None;
        }
        return Some(path.to_string());
    }
    if pattern.contains("://") {
        return None;
    }
    if pattern.starts_with('/') {
        return Some(pattern.to_string());
    }
    let dir = &dict_path[..dict_path.rfind('/').map_or(0, |i| i + 1)];
    Some(format!("{dir}{pattern}"))
}

fn has_named_group(pattern: &str) -> bool {
    let b = pattern.as_bytes();
    b.windows(2)
        .any(|w| w[0] == b':' && (w[1].is_ascii_alphabetic() || w[1] == b'_'))
}

/// Whether `path` matches the resolved pattern: `*` is any run of
/// characters, `\x` is `x`, anything else itself.
pub fn pattern_matches(pattern: &str, path: &str) -> bool {
    // Split into literal pieces around unescaped stars.
    let mut pieces = vec![String::new()];
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(e) => pieces.last_mut().unwrap().push(e),
                None => return false,
            },
            '*' => pieces.push(String::new()),
            _ => pieces.last_mut().unwrap().push(c),
        }
    }
    let (first, rest) = pieces.split_first().unwrap();
    let Some(mut tail) = path.strip_prefix(first.as_str()) else {
        return false;
    };
    let Some((last, middle)) = rest.split_last() else {
        return tail.is_empty();
    };
    for piece in middle {
        match tail.find(piece.as_str()) {
            Some(i) => tail = &tail[i + piece.len()..],
            None => return false,
        }
    }
    tail.ends_with(last.as_str())
}

/// A stored dictionary.
#[derive(Debug)]
pub struct Dictionary {
    pub hash: Hash,
    pub bytes: Bytes,
    pub host: String,
    /// The pattern, resolved to a path pattern.
    pub pattern: String,
    pub dest: Vec<String>,
    pub id: Option<String>,
    pub expires: SystemTime,
}

impl Dictionary {
    /// Whether a request for `path` on `host` with `headers` may use it.
    fn applies(&self, host: &str, path: &str, headers: &HeaderMap, now: SystemTime) -> bool {
        if now >= self.expires || !self.host.eq_ignore_ascii_case(host) {
            return false;
        }
        let path = path.split(['?', '#']).next().unwrap_or(path);
        if !pattern_matches(&self.pattern, path) {
            return false;
        }
        if !self.dest.is_empty() {
            let dest = headers
                .get("sec-fetch-dest")
                .and_then(|v| v.to_str().ok())
                .map(str::trim);
            if !dest.is_some_and(|d| self.dest.iter().any(|x| x == d)) {
                return false;
            }
        }
        // A client that echoes an id must echo this dictionary's.
        match (headers.get(DICTIONARY_ID), &self.id) {
            (None, _) => true,
            (Some(v), Some(id)) => v
                .to_str()
                .ok()
                .and_then(|t| sf_string(t.trim()))
                .is_some_and(|(s, r)| &s == id && r.is_empty()),
            (Some(_), None) => false,
        }
    }
}

/// Limits of the dictionary store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictionarySettings {
    /// The largest dictionary kept.
    pub max_dictionary_bytes: usize,
    /// All dictionaries together.
    pub max_total_bytes: usize,
    /// The longest a dictionary is kept, however long its response is fresh.
    pub max_ttl: Duration,
    /// Dictionary-compressed bodies kept, all together.
    pub max_compressed_bytes: usize,
}

impl Default for DictionarySettings {
    fn default() -> Self {
        DictionarySettings {
            max_dictionary_bytes: 4 << 20,
            max_total_bytes: 64 << 20,
            max_ttl: Duration::from_secs(30 * 86_400),
            max_compressed_bytes: 64 << 20,
        }
    }
}

type MemoKey = (Key, Hash, DictCoding);

#[derive(Default)]
struct Inner {
    dicts: HashMap<Hash, Arc<Dictionary>>,
    total: usize,
    memo: HashMap<MemoKey, (Weak<CachedObject>, Bytes)>,
    order: VecDeque<MemoKey>,
    memo_bytes: usize,
}

/// The dictionaries and the bodies compressed with them.
#[derive(Default)]
pub struct Dictionaries {
    settings: DictionarySettings,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for Dictionaries {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f.debug_struct("Dictionaries")
            .field("dictionaries", &inner.dicts.len())
            .field("bytes", &inner.total)
            .field("compressed", &inner.memo.len())
            .finish()
    }
}

impl Dictionaries {
    pub fn new(settings: DictionarySettings) -> Self {
        Dictionaries {
            settings,
            inner: Mutex::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Dictionaries held and their bytes.
    pub fn len(&self) -> (usize, usize) {
        let inner = self.lock();
        (inner.dicts.len(), inner.total)
    }

    /// Keeps the stored response `obj` of `key` as a dictionary when it
    /// offers itself as one, and returns its hash.
    pub fn record(&self, key: &Key, obj: &CachedObject, now: SystemTime) -> Option<Hash> {
        if obj.status != http::StatusCode::OK
            || obj.coding != Some(crate::encoding::Coding::Identity)
            || obj.derived
            || obj.body.is_empty()
            || obj.body.len() > self.settings.max_dictionary_bytes
        {
            return None;
        }
        let offer = obj.headers.get(USE_AS_DICTIONARY)?.to_str().ok()?;
        let offer = parse_use_as_dictionary(offer)?;
        let (host, path) = key.split_at(key.find('/')?);
        let pattern = resolve(&offer.pattern, host, path)?;
        let ttl = obj
            .freshness
            .lifetime
            .saturating_sub(obj.freshness.current_age(now));
        if ttl.is_zero() {
            return None;
        }
        let dict = Arc::new(Dictionary {
            hash: hash(&obj.body),
            bytes: obj.body.clone(),
            host: host.to_string(),
            pattern,
            dest: offer.dest,
            id: offer.id,
            expires: now + ttl.min(self.settings.max_ttl),
        });
        let h = dict.hash;
        let mut inner = self.lock();
        inner.dicts.retain(|_, d| d.expires > now);
        if let Some(old) = inner.dicts.insert(h, dict.clone()) {
            inner.total -= old.bytes.len();
        }
        inner.total += dict.bytes.len();
        // Over budget: the ones that would expire first go.
        while inner.total > self.settings.max_total_bytes {
            let Some(soonest) = inner
                .dicts
                .values()
                .filter(|d| d.hash != h)
                .min_by_key(|d| d.expires)
                .map(|d| d.hash)
            else {
                break;
            };
            if let Some(gone) = inner.dicts.remove(&soonest) {
                inner.total -= gone.bytes.len();
            }
        }
        inner.total = inner.dicts.values().map(|d| d.bytes.len()).sum();
        Some(h)
    }

    /// The dictionary and coding a request for `key` may be answered with.
    pub fn select(
        &self,
        key: &Key,
        headers: &HeaderMap,
        now: SystemTime,
    ) -> Option<(Arc<Dictionary>, DictCoding)> {
        let coding = accepted(headers)?;
        let wanted = available(headers)?;
        let (host, path) = key.split_at(key.find('/')?);
        let dict = self.lock().dicts.get(&wanted).cloned()?;
        dict.applies(host, path, headers, now)
            .then_some((dict, coding))
    }

    /// The body of `source` compressed with `dict`, if it was kept.
    pub fn compressed(
        &self,
        key: &Key,
        source: &Arc<CachedObject>,
        dict: &Dictionary,
        coding: DictCoding,
    ) -> Option<Bytes> {
        let inner = self.lock();
        let (from, body) = inner.memo.get(&(key.clone(), dict.hash, coding))?;
        (from.as_ptr() == Arc::as_ptr(source)).then(|| body.clone())
    }

    /// Keeps a compressed body for [`Dictionaries::compressed`], dropping
    /// the oldest beyond the budget.
    pub fn keep(
        &self,
        key: &Key,
        source: &Arc<CachedObject>,
        dict: &Dictionary,
        coding: DictCoding,
        body: Bytes,
    ) {
        if body.len() > self.settings.max_compressed_bytes {
            return;
        }
        let mut inner = self.lock();
        let k = (key.clone(), dict.hash, coding);
        inner.memo_bytes += body.len();
        if let Some((_, old)) = inner.memo.insert(k.clone(), (Arc::downgrade(source), body)) {
            inner.memo_bytes -= old.len();
            inner.order.retain(|o| o != &k);
        }
        inner.order.push_back(k);
        while inner.memo_bytes > self.settings.max_compressed_bytes {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            if let Some((_, gone)) = inner.memo.remove(&oldest) {
                inner.memo_bytes -= gone.len();
            }
        }
    }
}

/// Compresses `data` with `dict` in `coding`, header included.
#[cfg(feature = "compression")]
pub fn compress(
    coding: DictCoding,
    dict: &Dictionary,
    data: &[u8],
    settings: &crate::variants::CompressionSettings,
) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / 8 + coding.header_len());
    out.extend_from_slice(coding.magic());
    out.extend_from_slice(&dict.hash);
    match coding {
        DictCoding::Dcb => {
            use brotli::enc::{BrotliEncoderParams, StandardAlloc};
            let params = BrotliEncoderParams {
                quality: settings.brotli_quality.min(11) as i32,
                // The window must reach back over the dictionary.
                lgwin: window_log(dict.bytes.len() + data.len()).clamp(16, 24) as i32,
                size_hint: data.len(),
                ..Default::default()
            };
            let mut input = brotli::IoReaderWrapper(&mut &data[..]);
            let mut output = brotli::IoWriterWrapper(&mut out);
            let (mut ib, mut ob) = (vec![0u8; 4096], vec![0u8; 4096]);
            brotli::enc::BrotliCompressCustomIoCustomDict(
                &mut input,
                &mut output,
                &mut ib,
                &mut ob,
                &params,
                StandardAlloc::default(),
                &mut |_, _, _, _| (),
                &dict.bytes,
                std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
            )?;
        }
        DictCoding::Dcz => {
            let mut enc = zstd::bulk::Compressor::with_dictionary(
                settings.zstd_level.clamp(1, 19),
                &dict.bytes,
            )?;
            enc.set_parameter(zstd::zstd_safe::CParameter::WindowLog(
                window_log(dict.bytes.len() + data.len()).clamp(17, 27),
            ))?;
            out.extend_from_slice(&enc.compress(data)?);
        }
    }
    Ok(out)
}

/// The base-2 log of the smallest power of two at least `n`.
#[cfg(feature = "compression")]
fn window_log(n: usize) -> u32 {
    usize::BITS - n.max(1).saturating_sub(1).leading_zeros()
}

/// Decompresses a `coding` body made with `dict`, checking its header,
/// and reads at most `limit` bytes. For clients: the tests and the demo.
#[cfg(feature = "compression")]
pub fn decompress(
    coding: DictCoding,
    dict: &[u8],
    body: &[u8],
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let bad = |what| std::io::Error::new(std::io::ErrorKind::InvalidData, what);
    let rest = body
        .strip_prefix(coding.magic())
        .ok_or_else(|| bad("wrong magic"))?;
    if rest.len() < 32 || rest[..32] != hash(dict) {
        return Err(bad("made with another dictionary"));
    }
    let stream = &rest[32..];
    let mut out = Vec::new();
    let cap = limit as u64 + 1;
    match coding {
        DictCoding::Dcb => {
            let mut reader = brotli::reader::Decompressor::new_with_custom_dict(
                stream,
                4096,
                dict.to_vec().into(),
            );
            (&mut reader).take(cap).read_to_end(&mut out)?;
        }
        DictCoding::Dcz => {
            zstd::stream::read::Decoder::with_dictionary(stream, dict)?
                .take(cap)
                .read_to_end(&mut out)?;
        }
    }
    if out.len() > limit {
        return Err(bad("decompressed body over the limit"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use http::HeaderValue;

    use super::*;
    use crate::object::tests::map;
    use crate::policy::Freshness;

    #[test]
    fn use_as_dictionary_is_read_as_a_structured_dictionary() {
        let d = parse_use_as_dictionary(
            r#"match="/js/app.*.js", match-dest=("script" "worker"), id="dict-\"1\"", type=raw"#,
        )
        .unwrap();
        assert_eq!(d.pattern, "/js/app.*.js");
        assert_eq!(d.dest, ["script", "worker"]);
        assert_eq!(d.id.as_deref(), Some("dict-\"1\""));
        let plain = parse_use_as_dictionary(r#"match="/a/*",future=?1,other="x""#).unwrap();
        assert_eq!(
            (plain.pattern.as_str(), plain.dest.len(), plain.id),
            ("/a/*", 0, None)
        );
        for bad in [
            "",
            r#"id="x""#,
            r#"match=/a"#,
            r#"match="/a", type=custom"#,
            r#"match="""#,
            r#"match="/a" junk"#,
            r#"match="/a", match-dest="script""#,
            r#"match="unterminated"#,
            r#"match="/\x""#,
        ] {
            assert!(parse_use_as_dictionary(bad).is_none(), "{bad}");
        }
        let long = format!(r#"match="/a", id="{}""#, "x".repeat(MAX_ID + 1));
        assert!(parse_use_as_dictionary(&long).is_none());
    }

    #[test]
    fn hashes_travel_as_structured_byte_sequences() {
        for data in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            assert_eq!(unbase64(&base64(data)).unwrap(), data);
        }
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"fo"), "Zm8=");
        for bad in ["Zm9", "Zm=v", "Z===", "Zm9v!A=="] {
            assert!(unbase64(bad).is_none(), "{bad}");
        }
        let h = hash(b"dictionary");
        let req = map(&[("available-dictionary", &format!(":{}:", base64(&h)))]);
        assert_eq!(available(&req), Some(h));
        assert!(available(&map(&[("available-dictionary", &base64(&h))])).is_none());
        assert!(available(&map(&[("available-dictionary", ":Zm9v:")])).is_none());
        let mut two = req.clone();
        two.append(AVAILABLE_DICTIONARY, HeaderValue::from_static(":Zm9v:"));
        assert!(available(&two).is_none());
    }

    #[test]
    fn dictionary_codings_are_read_from_accept_encoding() {
        let ae = |v: &str| accepted(&map(&[("accept-encoding", v)]));
        assert_eq!(ae("gzip, br, zstd, dcb, dcz"), Some(DictCoding::Dcb));
        assert_eq!(ae("dcz, dcb;q=0.5"), Some(DictCoding::Dcz));
        assert_eq!(ae("DCZ"), Some(DictCoding::Dcz));
        assert_eq!(ae("dcz;q=0.5, dcb;q=0.5"), Some(DictCoding::Dcb));
        assert_eq!(ae("br, *"), None);
        assert_eq!(ae("dcb;q=0"), None);
        assert_eq!(ae("dcb;q=2, dcz;x"), None);
        assert_eq!(accepted(&map(&[])), None);
    }

    #[test]
    fn patterns_resolve_against_the_dictionary_and_match_paths() {
        assert_eq!(resolve("/js/*", "a.example", "/x.js").unwrap(), "/js/*");
        assert_eq!(
            resolve("app.*.js", "a.example", "/js/app.1.js").unwrap(),
            "/js/app.*.js"
        );
        assert_eq!(
            resolve("https://a.example/js/*", "a.example", "/").unwrap(),
            "/js/*"
        );
        assert!(resolve("https://b.example/js/*", "a.example", "/").is_none());
        assert!(resolve("ftp://a.example/*", "a.example", "/").is_none());
        for unsupported in [
            "/js/:name.js",
            "/js/(\\d+).js",
            "/js/{a,b}",
            "/js/*?v=1",
            "/a+",
        ] {
            assert!(
                resolve(unsupported, "a.example", "/").is_none(),
                "{unsupported}"
            );
        }
        assert!(pattern_matches("/js/app.*.js", "/js/app.v2.js"));
        assert!(pattern_matches("/js/app.*.js", "/js/app..js"));
        assert!(!pattern_matches("/js/app.*.js", "/js/app.v2.css"));
        assert!(!pattern_matches("/js/app.*.js", "/css/app.v2.js"));
        assert!(pattern_matches("/*", "/"));
        assert!(pattern_matches("/a/*/b/*", "/a/1/b/2"));
        assert!(!pattern_matches("/a/*/b/*", "/a/1/c/2"));
        assert!(pattern_matches("/exact", "/exact"));
        assert!(!pattern_matches("/exact", "/exact/more"));
        assert!(pattern_matches("/lit\\*", "/lit*"));
        assert!(!pattern_matches("/lit\\*", "/litx"));
        assert!(!pattern_matches("/bad\\", "/bad"));
    }

    fn dict_object(offer: &str, body: &'static str, lifetime: u64) -> CachedObject {
        CachedObject::new(
            http::StatusCode::OK,
            map(&[
                ("use-as-dictionary", offer),
                ("content-type", "text/javascript"),
            ]),
            Bytes::from_static(body.as_bytes()),
            Freshness {
                lifetime: Duration::from_secs(lifetime),
                corrected_initial_age: Duration::ZERO,
                response_time: SystemTime::UNIX_EPOCH,
            },
            &map(&[]),
        )
    }

    fn request(h: &Hash, extra: &[(&str, &str)]) -> HeaderMap {
        let mut req = map(extra);
        req.insert(
            AVAILABLE_DICTIONARY,
            HeaderValue::from_str(&format!(":{}:", base64(h))).unwrap(),
        );
        req.append("accept-encoding", HeaderValue::from_static("gzip, br, dcb"));
        req
    }

    #[test]
    fn dictionaries_are_matched_by_hash_host_pattern_destination_and_id() {
        let t0 = SystemTime::UNIX_EPOCH;
        let d = Dictionaries::default();
        let key: Key = "a.example/js/app.v1.js".into();
        let offer = r#"match="app.*.js", match-dest=("script"), id="v1""#;
        let h = d
            .record(&key, &dict_object(offer, "dict body", 60), t0)
            .unwrap();
        assert_eq!(h, hash(b"dict body"));
        assert_eq!(d.len(), (1, 9));
        let v2: Key = "a.example/js/app.v2.js".into();
        let script = [("sec-fetch-dest", "script")];
        let (got, coding) = d.select(&v2, &request(&h, &script), t0).unwrap();
        assert_eq!((got.hash, coding), (h, DictCoding::Dcb));
        let id = [("sec-fetch-dest", "script"), ("dictionary-id", "\"v1\"")];
        assert!(d.select(&v2, &request(&h, &id), t0).is_some());
        let wrong_id = [("sec-fetch-dest", "script"), ("dictionary-id", "\"v0\"")];
        assert!(d.select(&v2, &request(&h, &wrong_id), t0).is_none());
        // Another destination, none, another host, another path, another
        // hash, no dictionary coding, or after it expired: not used.
        assert!(
            d.select(&v2, &request(&h, &[("sec-fetch-dest", "style")]), t0)
                .is_none()
        );
        assert!(d.select(&v2, &request(&h, &[]), t0).is_none());
        let other_host: Key = "b.example/js/app.v2.js".into();
        assert!(d.select(&other_host, &request(&h, &script), t0).is_none());
        let css: Key = "a.example/js/app.v2.css".into();
        assert!(d.select(&css, &request(&h, &script), t0).is_none());
        assert!(
            d.select(&v2, &request(&hash(b"other"), &script), t0)
                .is_none()
        );
        let mut no_dcb = request(&h, &script);
        no_dcb.insert("accept-encoding", HeaderValue::from_static("br"));
        assert!(d.select(&v2, &no_dcb, t0).is_none());
        assert!(
            d.select(&v2, &request(&h, &script), t0 + Duration::from_secs(60))
                .is_none()
        );
    }

    #[test]
    fn only_fresh_identity_offers_within_budget_become_dictionaries() {
        let t0 = SystemTime::UNIX_EPOCH;
        let d = Dictionaries::new(DictionarySettings {
            max_dictionary_bytes: 8,
            max_total_bytes: 12,
            max_ttl: Duration::from_secs(10),
            max_compressed_bytes: 10,
        });
        let key: Key = "a.example/d".into();
        assert!(
            d.record(&key, &dict_object(r#"match="/*""#, "123456789", 60), t0)
                .is_none(),
            "too big"
        );
        assert!(
            d.record(&key, &dict_object(r#"match="/*""#, "1234", 0), t0)
                .is_none(),
            "stale"
        );
        assert!(
            d.record(&key, &dict_object(r#"type=x"#, "1234", 60), t0)
                .is_none(),
            "bad offer"
        );
        let first = d
            .record(&key, &dict_object(r#"match="/*""#, "11111111", 60), t0)
            .unwrap();
        // The TTL is capped at max_ttl.
        let req = request(&first, &[]);
        assert!(
            d.select(&"a.example/x".into(), &req, t0 + Duration::from_secs(9))
                .is_some()
        );
        assert!(
            d.select(&"a.example/x".into(), &req, t0 + Duration::from_secs(10))
                .is_none()
        );
        // Over the total, the one expiring first goes.
        let second = d
            .record(
                &key,
                &dict_object(r#"match="/*""#, "2222", 60),
                t0 + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(d.len(), (2, 12));
        d.record(
            &key,
            &dict_object(r#"match="/*""#, "33333", 60),
            t0 + Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(d.len(), (2, 9));
        assert!(
            d.select(&"a.example/x".into(), &request(&first, &[]), t0)
                .is_none()
        );
        assert!(
            d.select(
                &"a.example/x".into(),
                &request(&second, &[]),
                t0 + Duration::from_secs(2)
            )
            .is_some()
        );
    }

    #[test]
    fn compressed_bodies_are_kept_for_their_source_within_budget() {
        let d = Dictionaries::new(DictionarySettings {
            max_compressed_bytes: 10,
            ..DictionarySettings::default()
        });
        let key: Key = "a.example/x".into();
        let dict = Dictionary {
            hash: hash(b"d"),
            bytes: Bytes::from_static(b"d"),
            host: "a.example".into(),
            pattern: "/*".into(),
            dest: vec![],
            id: None,
            expires: SystemTime::UNIX_EPOCH,
        };
        let source = Arc::new(dict_object(r#"match="/*""#, "s", 60));
        d.keep(
            &key,
            &source,
            &dict,
            DictCoding::Dcb,
            Bytes::from_static(b"123456"),
        );
        assert_eq!(
            d.compressed(&key, &source, &dict, DictCoding::Dcb).unwrap(),
            "123456"
        );
        assert!(
            d.compressed(&key, &source, &dict, DictCoding::Dcz)
                .is_none()
        );
        let newer = Arc::new(dict_object(r#"match="/*""#, "s2", 60));
        assert!(
            d.compressed(&key, &newer, &dict, DictCoding::Dcb).is_none(),
            "another source"
        );
        d.keep(
            &key,
            &source,
            &dict,
            DictCoding::Dcz,
            Bytes::from_static(b"7890"),
        );
        assert!(
            d.compressed(&key, &source, &dict, DictCoding::Dcz)
                .is_some()
        );
        // A newer source's body replaces the old one's.
        d.keep(
            &key,
            &newer,
            &dict,
            DictCoding::Dcb,
            Bytes::from_static(b"abcdef"),
        );
        assert!(
            d.compressed(&key, &source, &dict, DictCoding::Dcb)
                .is_none()
        );
        assert!(d.compressed(&key, &newer, &dict, DictCoding::Dcb).is_some());
        assert!(
            d.compressed(&key, &source, &dict, DictCoding::Dcz)
                .is_some()
        );
        // Over budget, the oldest goes.
        let other: Key = "a.example/y".into();
        d.keep(
            &other,
            &source,
            &dict,
            DictCoding::Dcb,
            Bytes::from_static(b"12"),
        );
        assert!(
            d.compressed(&key, &source, &dict, DictCoding::Dcz)
                .is_none()
        );
        assert!(
            d.compressed(&other, &source, &dict, DictCoding::Dcb)
                .is_some()
        );
        d.keep(
            &key,
            &newer,
            &dict,
            DictCoding::Dcz,
            Bytes::from(vec![0; 11]),
        );
        assert!(
            d.compressed(&key, &newer, &dict, DictCoding::Dcz).is_none(),
            "larger than the budget"
        );
    }

    #[cfg(feature = "compression")]
    #[test]
    fn dcb_and_dcz_round_trip_and_check_their_dictionary() {
        let v1: String = (0..2000)
            .map(|i| format!("export const v{i} = {};\n", i * 7))
            .collect();
        let v2 = v1.replace("v1999 =", "v1999 = 1 +");
        let dict = Dictionary {
            hash: hash(v1.as_bytes()),
            bytes: Bytes::from(v1.clone()),
            host: "a.example".into(),
            pattern: "/*".into(),
            dest: vec![],
            id: None,
            expires: SystemTime::UNIX_EPOCH,
        };
        let settings = crate::variants::CompressionSettings::default();
        let plain =
            crate::variants::compress(crate::encoding::Coding::Br, v2.as_bytes(), &settings)
                .unwrap();
        for coding in [DictCoding::Dcb, DictCoding::Dcz] {
            let body = compress(coding, &dict, v2.as_bytes(), &settings).unwrap();
            assert_eq!(
                &body[coding.header_len() - 32..coding.header_len()],
                &dict.hash
            );
            assert!(
                body.len() * 10 < plain.len(),
                "{coding:?}: {} against br {}",
                body.len(),
                plain.len()
            );
            assert_eq!(
                decompress(coding, v1.as_bytes(), &body, v2.len()).unwrap(),
                v2.as_bytes()
            );
            assert!(decompress(coding, b"another", &body, v2.len()).is_err());
            assert!(decompress(coding, v1.as_bytes(), &body, v2.len() - 1).is_err());
            assert!(decompress(coding, v1.as_bytes(), &body[1..], v2.len()).is_err());
        }
    }
}
