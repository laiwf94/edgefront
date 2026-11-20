#![no_main]

use edgefront_cache::encoding::{Accepted, Coding, negotiate, normalise};
use http::header::{ACCEPT_ENCODING, HeaderMap, HeaderValue};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Each line of the input is one Accept-Encoding field line.
    let mut headers = HeaderMap::new();
    for line in data.split(|&b| b == b'\n') {
        if let Ok(v) = HeaderValue::from_bytes(line) {
            headers.append(ACCEPT_ENCODING, v);
        }
    }
    let accepted = Accepted::from_headers(&headers);
    for c in Coding::PREFERENCE {
        assert!(accepted.weight(c) <= 1000);
    }
    let chosen = normalise(&headers);
    // What it chooses is acceptable, and the same as asking with all four.
    if let Some(c) = chosen {
        assert!(accepted.weight(c) > 0 || !headers.contains_key(ACCEPT_ENCODING));
    }
    assert_eq!(chosen, negotiate(&headers, &Coding::PREFERENCE));
    // Narrowing what is available never yields a coding outside it.
    for c in Coding::PREFERENCE {
        assert!(negotiate(&headers, &[c]).is_none_or(|got| got == c));
    }
});
