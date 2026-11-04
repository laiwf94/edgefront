//! Request IDs, ported from steer's `internal/reqid`.
//!
//! An inbound `X-Request-Id` is kept when it is safe to log and forward:
//! 1 to [`MAX_LENGTH`] bytes from `A-Z a-z 0-9 . _ : -`. Otherwise the
//! proxy makes a new one of 24 hex characters, as steer did.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicU64, Ordering};

use http::HeaderValue;
use http::header::HeaderName;

/// The header that carries the ID in both directions.
pub const HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// The longest inbound ID accepted.
pub const MAX_LENGTH: usize = 128;

/// Whether `id` is an acceptable inbound request ID (steer's `Valid`).
pub fn valid(id: &[u8]) -> bool {
    (1..=MAX_LENGTH).contains(&id.len())
        && id
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b':' | b'-'))
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A new ID: 96 bits as 24 lower-case hex characters, the shape of steer's
/// `New`. The bits are keyed SipHashes of a process-wide counter, so IDs
/// never repeat within a process and are hard to guess across processes.
pub fn new_id() -> HeaderValue {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let a = RandomState::new().hash_one(n);
    let b = RandomState::new().hash_one((n, a)) as u32;
    let id = format!("{a:016x}{b:08x}");
    HeaderValue::try_from(id).expect("hex is a valid header value")
}

/// The ID for a request: the first inbound `X-Request-Id` if it is
/// [`valid`], a [`new_id`] otherwise (steer's `FromRequest`).
pub fn from_request(headers: &http::HeaderMap) -> HeaderValue {
    match headers.get(&HEADER) {
        Some(v) if valid(v.as_bytes()) => v.clone(),
        _ => new_id(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn steer_vectors() {
        // reqid_test.go's TestFromRequest and FuzzValid's invariant.
        let mut h = http::HeaderMap::new();
        h.insert(HEADER, HeaderValue::from_static("abc-123.x_y:z"));
        assert_eq!(from_request(&h), "abc-123.x_y:z");
        let long = "a".repeat(129);
        for bad in ["", "has space", "<script>", &long] {
            h.insert(HEADER, HeaderValue::from_str(bad).unwrap());
            let got = from_request(&h);
            assert_ne!(got, bad);
            assert_eq!(got.len(), 24);
            assert!(valid(got.as_bytes()));
        }
        // "new\nline" and "ü" cannot be header values in hyper, but the
        // check refuses their bytes all the same.
        assert!(!valid(b"new\nline"));
        assert!(!valid("\u{fc}".as_bytes()));
        assert!(valid(&[b'a'; 128]));
        for c in b" \r\n\t\"<>,;" {
            assert!(!valid(&[b'a', *c]));
        }
    }

    #[test]
    fn new_ids_are_unique_hex() {
        let ids: HashSet<HeaderValue> = (0..10_000).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 10_000);
        for id in &ids {
            assert_eq!(id.len(), 24);
            assert!(
                id.as_bytes()
                    .iter()
                    .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
            );
        }
    }

    #[test]
    fn inbound_ids_are_kept_only_when_valid() {
        let mut h = http::HeaderMap::new();
        assert_eq!(from_request(&h).len(), 24);
        h.insert(HEADER, HeaderValue::from_static("trace:1.2-3_4"));
        assert_eq!(from_request(&h), "trace:1.2-3_4");
        // Only the first value counts, as with Go's Header.Get.
        h.append(HEADER, HeaderValue::from_static("second"));
        assert_eq!(from_request(&h), "trace:1.2-3_4");
        h.insert(HEADER, HeaderValue::from_static("not valid"));
        assert_eq!(from_request(&h).len(), 24);
    }
}
