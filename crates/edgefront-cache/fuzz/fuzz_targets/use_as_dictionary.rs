#![no_main]

use edgefront_cache::dictionary::{parse_use_as_dictionary, pattern_matches};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The first line is a Use-As-Dictionary value, the second a path.
    let mut lines = data.splitn(2, |&b| b == b'\n');
    let Ok(value) = std::str::from_utf8(lines.next().unwrap_or_default()) else {
        return;
    };
    let path = std::str::from_utf8(lines.next().unwrap_or_default()).unwrap_or("/");
    if let Some(offer) = parse_use_as_dictionary(value) {
        assert!(!offer.pattern.is_empty() && offer.pattern.len() <= 2048);
        assert!(offer.id.as_ref().is_none_or(|id| id.len() <= 1024));
        // Matching never panics, and a pattern without stars or escapes
        // matches exactly itself.
        let _ = pattern_matches(&offer.pattern, path);
        if !offer.pattern.contains(['*', '\\']) {
            assert!(pattern_matches(&offer.pattern, &offer.pattern));
        }
    }
});
