#![no_main]

use edgefront_cache::cluster::{PurgeMessage, sign, verify};
use edgefront_cache::purge::parse_request;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = parse_request(data);
    let _ = serde_json::from_slice::<PurgeMessage>(data);
    let key = b"fuzz key fuzz key fuzz key fuzz!";
    assert!(verify(key, data, sign(key, data).as_bytes()));
    let _ = verify(key, b"body", data);
});
