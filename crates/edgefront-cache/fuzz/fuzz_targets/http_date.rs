#![no_main]

use edgefront_cache::headers::{http_date, parse_http_date};
use http::HeaderValue;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(value) = HeaderValue::from_bytes(data) else {
        return;
    };
    if let Some(t) = parse_http_date(&value) {
        // Whatever parses formats as an IMF-fixdate that parses back.
        assert_eq!(parse_http_date(&http_date(t)), Some(t));
    }
});
