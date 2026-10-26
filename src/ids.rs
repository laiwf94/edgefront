//! Request and message identifiers.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicU64, Ordering};

use http::HeaderMap;
use http::header::{HeaderName, HeaderValue};

pub const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A 128-bit identifier as 32 hex characters. Each half is a keyed SipHash
/// of a process-wide counter, so ids are unique within a process and hard
/// to guess across processes. They correlate logs; they are not secrets.
pub fn new_id() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let a = RandomState::new().hash_one(n);
    let b = RandomState::new().hash_one((n, a));
    format!("{a:016x}{b:016x}")
}

/// Keeps a caller's `x-request-id` when it is short and plain, so ids can
/// be followed across systems; otherwise makes a new one. Anything else
/// would let a client put arbitrary text into our logs.
pub fn request_id(headers: &HeaderMap) -> HeaderValue {
    if let Some(v) = headers.get(REQUEST_ID) {
        let b = v.as_bytes();
        if (1..=64).contains(&b.len())
            && b.iter()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(c))
        {
            return v.clone();
        }
    }
    HeaderValue::from_str(&new_id()).expect("hex is a valid header value")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn ids_are_unique_hex() {
        let ids: HashSet<String> = (0..10_000).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 10_000);
        assert!(
            ids.iter()
                .all(|i| i.len() == 32 && i.bytes().all(|b| b.is_ascii_hexdigit()))
        );
    }

    #[test]
    fn caller_ids_are_kept_only_when_plain() {
        let mut h = HeaderMap::new();
        h.insert(REQUEST_ID, HeaderValue::from_static("abc-123_x.y"));
        assert_eq!(request_id(&h), "abc-123_x.y");
        for bad in ["", "has space", "quote\"", &"x".repeat(65)] {
            h.insert(REQUEST_ID, HeaderValue::from_str(bad).unwrap());
            let id = request_id(&h);
            assert_eq!(id.len(), 32, "{bad:?}");
        }
        assert_eq!(request_id(&HeaderMap::new()).len(), 32);
    }
}
