#![no_main]

use edgefront_core::ids::{REQUEST_ID, request_id};
use http::{HeaderMap, HeaderValue};
use libfuzzer_sys::fuzz_target;

/// What may reach the logs: 1 to 64 bytes of `[A-Za-z0-9._-]`.
fn plain(b: &[u8]) -> bool {
    (1..=64).contains(&b.len())
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(c))
}

fuzz_target!(|data: &[u8]| {
    let mut headers = HeaderMap::new();
    let given = HeaderValue::from_bytes(data).ok();
    if let Some(v) = &given {
        headers.insert(REQUEST_ID, v.clone());
    }
    let id = request_id(&headers);
    assert!(plain(id.as_bytes()));
    match given {
        // A plain caller id is kept as it is.
        Some(v) if plain(v.as_bytes()) => assert_eq!(id, v),
        // Anything else is replaced by a fresh 32-hex-digit id.
        _ => assert!(id.len() == 32 && id.as_bytes().iter().all(u8::is_ascii_hexdigit)),
    }
});
