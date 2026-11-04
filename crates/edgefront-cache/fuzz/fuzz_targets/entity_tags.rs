#![no_main]

use edgefront_cache::headers::{EntityTag, EtagMatch};
use http::header::{HeaderMap, HeaderValue, IF_NONE_MATCH};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Some(tag) = EntityTag::parse(data) {
        // A parsed tag prints back to a value that parses to itself.
        let printed = tag.to_header_value();
        assert_eq!(EntityTag::parse(printed.as_bytes()), Some(tag.clone()));
        assert!(tag.weak_eq(&tag));
        assert_eq!(tag.strong_eq(&tag), !tag.weak);
    }
    let mut headers = HeaderMap::new();
    for line in data.split(|&b| b == b'\n') {
        if let Ok(v) = HeaderValue::from_bytes(line) {
            headers.append(IF_NONE_MATCH, v);
        }
    }
    if let Some(EtagMatch::Tags(tags)) = EtagMatch::from_headers(&headers, &IF_NONE_MATCH) {
        assert!(!tags.is_empty());
    }
});
