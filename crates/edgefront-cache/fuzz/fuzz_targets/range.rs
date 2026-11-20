#![no_main]

use edgefront_cache::range::{ByteRange, MAX_RANGES, parse};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (u16, &[u8])| {
    let (len, value) = input;
    let len = u64::from(len);
    match parse(value, len) {
        ByteRange::Satisfiable { start, end } => assert!(start <= end && end < len),
        ByteRange::Multiple(ranges) => {
            // At least two parts, at most MAX_RANGES, each inside the
            // object, never overlapping, and together no longer than it.
            assert!(ranges.len() >= 2 && ranges.len() <= MAX_RANGES);
            let mut total = 0;
            for (i, &(start, end)) in ranges.iter().enumerate() {
                assert!(start <= end && end < len);
                total += end - start + 1;
                for &(s2, e2) in &ranges[i + 1..] {
                    assert!(end < s2 || e2 < start, "{ranges:?} overlap");
                }
            }
            assert!(total <= len);
        }
        ByteRange::Unsatisfiable | ByteRange::Ignore => {}
    }
});
