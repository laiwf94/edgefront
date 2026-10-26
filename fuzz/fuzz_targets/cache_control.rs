#![no_main]

use cachet::cache_control::{CacheControl, DELTA_SECONDS_MAX};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Split the input into several field lines, as a header map would hold.
    let lines: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
    let cc = CacheControl::parse(lines.iter().copied());
    for v in [
        cc.max_age,
        cc.s_maxage,
        cc.min_fresh,
        cc.stale_while_revalidate,
        cc.stale_if_error,
    ] {
        assert!(v.is_none_or(|n| n <= DELTA_SECONDS_MAX));
    }
    // Parsing is deterministic and the first occurrence wins, so parsing
    // the input twice in a row gives the same result as parsing it once.
    let twice = CacheControl::parse(lines.iter().chain(lines.iter()).copied());
    assert_eq!(cc, twice);
});
