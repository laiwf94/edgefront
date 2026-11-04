#![no_main]

use edgefront_core::config::loader::parse_base_url;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(url) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(authority) = parse_base_url(url) else {
        return;
    };
    // An accepted URL is only an authority over http, without credentials,
    // and writing it back out parses to the same authority.
    assert!(!authority.as_str().contains('@'));
    assert!(!authority.host().is_empty());
    let again = parse_base_url(&format!("http://{authority}")).expect("a printed base URL parses");
    assert_eq!(again, authority);
});
