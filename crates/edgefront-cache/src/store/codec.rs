//! The on-disk record format for one cache entry (every variant of a URL).
//!
//! ```text
//! magic        8 bytes  "CACHET\0\x01"
//! header_len   u32
//! header       key, variant count, then per variant: status, freshness,
//!              header fields, Vary selecting values, body length
//! header_sum   32 bytes  SHA-256 of everything before it
//! bodies       the variant bodies, back to back
//! body_sum     32 bytes  SHA-256 of the bodies
//! ```
//!
//! Integers are big-endian. The header has its own checksum so a startup
//! scan can rebuild the index without reading bodies; reads check both.
//! Every length is checked against the bytes actually present before use.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::StatusCode;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use sha2::{Digest, Sha256};

use crate::object::{CachedObject, Entry, Key, VaryValues};
use crate::policy::Freshness;

pub const MAGIC: &[u8; 8] = b"CACHET\0\x01";
const SUM: usize = 32;
/// Upper bound on a record header, far above anything a real entry needs.
pub(crate) const MAX_HEADER: usize = 1 << 20;
/// Bytes before the header: magic and header length.
pub(crate) const PREFIX: usize = MAGIC.len() + 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    Magic,
    Truncated,
    TooLarge,
    Checksum,
    Invalid,
}

/// The parts of a record the index needs.
#[derive(Debug, Clone)]
pub struct RecordHeader {
    pub key: Key,
    pub variants: Vec<VariantHeader>,
}

#[derive(Debug, Clone)]
pub struct VariantHeader {
    pub status: StatusCode,
    pub freshness: Freshness,
    pub headers: HeaderMap,
    pub vary: VaryValues,
    pub body_len: usize,
}

fn millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn duration_millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn put_bytes16(out: &mut Vec<u8>, b: &[u8]) {
    let len = u16::try_from(b.len()).expect("field fits in u16");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(b);
}

fn put_bytes32(out: &mut Vec<u8>, b: &[u8]) {
    let len = u32::try_from(b.len()).expect("field fits in u32");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(b);
}

/// Encodes an entry. Returns `None` if a field is too large for the format.
pub fn encode(key: &str, entry: &Entry) -> Option<Vec<u8>> {
    let mut header = Vec::new();
    if key.len() > usize::from(u16::MAX) || entry.variants.len() > usize::from(u16::MAX) {
        return None;
    }
    put_bytes16(&mut header, key.as_bytes());
    header.extend_from_slice(&(entry.variants.len() as u16).to_be_bytes());
    for v in &entry.variants {
        header.extend_from_slice(&v.status.as_u16().to_be_bytes());
        header.extend_from_slice(&duration_millis(v.freshness.lifetime).to_be_bytes());
        header.extend_from_slice(&duration_millis(v.freshness.corrected_initial_age).to_be_bytes());
        header.extend_from_slice(&millis(v.freshness.response_time).to_be_bytes());
        if v.headers.len() > usize::from(u16::MAX) || v.vary().len() > usize::from(u16::MAX) {
            return None;
        }
        header.extend_from_slice(&(v.headers.len() as u16).to_be_bytes());
        for (name, value) in &v.headers {
            put_bytes16(&mut header, name.as_str().as_bytes());
            put_bytes32(&mut header, value.as_bytes());
        }
        header.extend_from_slice(&(v.vary().len() as u16).to_be_bytes());
        for (name, value) in v.vary() {
            put_bytes16(&mut header, name.as_str().as_bytes());
            match value {
                Some(s) => {
                    header.push(1);
                    put_bytes32(&mut header, s.as_bytes());
                }
                None => header.push(0),
            }
        }
        header.extend_from_slice(&u32::try_from(v.body.len()).ok()?.to_be_bytes());
    }
    if header.len() > MAX_HEADER {
        return None;
    }
    let mut out = Vec::with_capacity(PREFIX + header.len() + SUM * 2 + entry.size());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(header.len() as u32).to_be_bytes());
    out.extend_from_slice(&header);
    let sum = Sha256::digest(&out);
    out.extend_from_slice(&sum);
    let mut body_hash = Sha256::new();
    for v in &entry.variants {
        out.extend_from_slice(&v.body);
        body_hash.update(&v.body);
    }
    out.extend_from_slice(&body_hash.finalize());
    Some(out)
}

/// Length of the header section from the record prefix, including the
/// prefix and header checksum: how much a startup scan must read.
pub(crate) fn header_span(prefix: &[u8]) -> Result<usize, DecodeError> {
    if prefix.len() < PREFIX {
        return Err(DecodeError::Truncated);
    }
    if &prefix[..MAGIC.len()] != MAGIC {
        return Err(DecodeError::Magic);
    }
    let len = u32::from_be_bytes(prefix[MAGIC.len()..PREFIX].try_into().expect("4 bytes")) as usize;
    if len > MAX_HEADER {
        return Err(DecodeError::TooLarge);
    }
    Ok(PREFIX + len + SUM)
}

struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.buf.len() < n {
            return Err(DecodeError::Truncated);
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("2 bytes"),
        ))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    fn bytes16(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.u16()?;
        self.take(usize::from(n))
    }

    fn bytes32(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

/// Decodes and verifies the header section (the first `header_span` bytes).
pub fn decode_header(record: &[u8]) -> Result<RecordHeader, DecodeError> {
    let span = header_span(record)?;
    if record.len() < span {
        return Err(DecodeError::Truncated);
    }
    let (covered, sum) = record[..span].split_at(span - SUM);
    if Sha256::digest(covered).as_slice() != sum {
        return Err(DecodeError::Checksum);
    }
    let mut r = Reader {
        buf: &covered[PREFIX..],
    };
    let key = std::str::from_utf8(r.bytes16()?).map_err(|_| DecodeError::Invalid)?;
    let count = r.u16()?;
    let mut variants = Vec::with_capacity(usize::from(count).min(64));
    for _ in 0..count {
        let status = StatusCode::from_u16(r.u16()?).map_err(|_| DecodeError::Invalid)?;
        let lifetime = Duration::from_millis(r.u64()?);
        let corrected_initial_age = Duration::from_millis(r.u64()?);
        let response_time = UNIX_EPOCH
            .checked_add(Duration::from_millis(r.u64()?))
            .ok_or(DecodeError::Invalid)?;
        let mut headers = HeaderMap::new();
        for _ in 0..r.u16()? {
            let name = HeaderName::from_bytes(r.bytes16()?).map_err(|_| DecodeError::Invalid)?;
            let value = HeaderValue::from_bytes(r.bytes32()?).map_err(|_| DecodeError::Invalid)?;
            headers.append(name, value);
        }
        let mut vary = Vec::new();
        for _ in 0..r.u16()? {
            let name = HeaderName::from_bytes(r.bytes16()?).map_err(|_| DecodeError::Invalid)?;
            let value = match r.u8()? {
                0 => None,
                1 => Some(
                    String::from_utf8(r.bytes32()?.to_vec()).map_err(|_| DecodeError::Invalid)?,
                ),
                _ => return Err(DecodeError::Invalid),
            };
            vary.push((name, value));
        }
        let body_len = r.u32()? as usize;
        variants.push(VariantHeader {
            status,
            freshness: Freshness {
                lifetime,
                corrected_initial_age,
                response_time,
            },
            headers,
            vary,
            body_len,
        });
    }
    if !r.buf.is_empty() {
        return Err(DecodeError::Invalid);
    }
    Ok(RecordHeader {
        key: key.into(),
        variants,
    })
}

/// Decodes and verifies a whole record.
pub fn decode(record: &[u8]) -> Result<(Key, Entry), DecodeError> {
    let header = decode_header(record)?;
    let span = header_span(record)?;
    let body_total: usize = header.variants.iter().map(|v| v.body_len).sum();
    if record.len() != span + body_total + SUM {
        return Err(DecodeError::Truncated);
    }
    let bodies = &record[span..span + body_total];
    if Sha256::digest(bodies).as_slice() != &record[span + body_total..] {
        return Err(DecodeError::Checksum);
    }
    let bodies = Bytes::copy_from_slice(bodies);
    let mut offset = 0;
    let mut variants = Vec::with_capacity(header.variants.len());
    for v in header.variants {
        let body = bodies.slice(offset..offset + v.body_len);
        offset += v.body_len;
        let obj = CachedObject::with_vary(v.status, v.headers, body, v.freshness, v.vary);
        variants.push(Arc::new(obj));
    }
    Ok((header.key, Entry { variants }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::tests::{map, object};

    fn sample() -> Entry {
        let a = object(
            &[
                ("vary", "accept-encoding"),
                ("etag", "\"a\""),
                ("surrogate-key", "t1"),
            ],
            &[("accept-encoding", "gzip")],
            "gzip body",
        );
        let b = object(&[("vary", "accept-encoding")], &[], "plain");
        Entry {
            variants: vec![a, b],
        }
    }

    #[test]
    fn round_trip() {
        let entry = sample();
        let bytes = encode("example.com/a", &entry).unwrap();
        let (key, back) = decode(&bytes).unwrap();
        assert_eq!(&*key, "example.com/a");
        assert_eq!(back.variants.len(), 2);
        for (x, y) in entry.variants.iter().zip(&back.variants) {
            assert_eq!(x.status, y.status);
            assert_eq!(x.headers, y.headers);
            assert_eq!(x.body, y.body);
            assert_eq!(x.vary(), y.vary());
            assert_eq!(x.freshness, y.freshness);
            assert_eq!(x.tags, y.tags);
        }
        assert!(back.find(&map(&[("accept-encoding", "gzip")])).is_some());
        let header = decode_header(&bytes[..header_span(&bytes).unwrap()]).unwrap();
        assert_eq!(header.variants[0].body_len, 9);
    }

    #[test]
    fn every_truncation_is_rejected() {
        let bytes = encode("k", &sample()).unwrap();
        for n in 0..bytes.len() {
            assert!(decode(&bytes[..n]).is_err(), "accepted a record cut at {n}");
        }
    }

    #[test]
    fn every_bit_flip_is_rejected() {
        let bytes = encode("k", &sample()).unwrap();
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 0x01;
            assert!(decode(&b).is_err(), "accepted a flip at byte {i}");
        }
    }

    #[test]
    fn foreign_and_oversized_headers() {
        assert_eq!(
            header_span(b"NOTCACHE\0\0\0\0").unwrap_err(),
            DecodeError::Magic
        );
        assert_eq!(header_span(b"CACHET").unwrap_err(), DecodeError::Truncated);
        let mut big = MAGIC.to_vec();
        big.extend_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(header_span(&big).unwrap_err(), DecodeError::TooLarge);
    }

    /// Builds a record with a valid checksum around an arbitrary header.
    fn with_header(header: &[u8]) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&(header.len() as u32).to_be_bytes());
        out.extend_from_slice(header);
        let sum = Sha256::digest(&out);
        out.extend_from_slice(&sum);
        out
    }

    #[test]
    fn checksummed_garbage_is_still_rejected() {
        let cases: Vec<Vec<u8>> = vec![
            vec![0, 1, 0xff],               // key is not UTF-8
            vec![0, 1, b'k', 0, 1, 0, 1],   // status 1 is invalid
            vec![0, 1, b'k', 0, 0, 9],      // trailing bytes
            vec![0, 1, b'k', 0, 1, 0, 200], // truncated variant
        ];
        for h in cases {
            assert!(decode_header(&with_header(&h)).is_err(), "{h:?}");
        }
        // An invalid header name and an invalid vary flag.
        let mut h = vec![0, 1, b'k', 0, 1, 0, 200];
        h.extend_from_slice(&[0; 24]);
        let mut bad_name = h.clone();
        bad_name.extend_from_slice(&[0, 1, 0, 1, b' ', 0, 0, 0, 0]);
        assert_eq!(
            decode_header(&with_header(&bad_name)).unwrap_err(),
            DecodeError::Invalid
        );
        let mut bad_flag = h.clone();
        bad_flag.extend_from_slice(&[0, 0, 0, 1, 0, 1, b'a', 7]);
        assert_eq!(
            decode_header(&with_header(&bad_flag)).unwrap_err(),
            DecodeError::Invalid
        );
    }

    #[test]
    fn oversized_fields_are_not_encoded() {
        let long_key = "k".repeat(70_000);
        assert!(encode(&long_key, &sample()).is_none());
    }

    #[test]
    fn times_before_the_epoch_clamp_to_zero() {
        assert_eq!(millis(UNIX_EPOCH - Duration::from_secs(1)), 0);
        assert_eq!(duration_millis(Duration::from_secs(2)), 2000);
    }
}
