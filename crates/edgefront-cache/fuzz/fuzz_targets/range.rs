#![no_main]

use edgefront_cache::range::{ByteRange, parse};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (u16, &[u8])| {
    let (len, value) = input;
    let len = u64::from(len);
    match parse(value, len) {
        ByteRange::Satisfiable { start, end } => assert!(start <= end && end < len),
        ByteRange::Unsatisfiable | ByteRange::Ignore => {}
    }
});
