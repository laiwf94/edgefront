#![no_main]

use edgefront_core::admin::BearerToken;
use http::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (&str, &[u8])| {
    let (token, given) = input;
    let bearer = BearerToken::new(token);
    // The comparison is over digests, so it matches exactly the token bytes.
    assert_eq!(bearer.matches(given), given == token.as_bytes());
    assert!(bearer.matches(token.as_bytes()));

    // Any Authorization header is either accepted or refused, never a panic,
    // and only `Bearer <token>` is accepted.
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_bytes(given) {
        headers.insert(AUTHORIZATION, v);
    }
    if bearer.check(&headers).is_ok() {
        assert_eq!(
            headers[AUTHORIZATION].as_bytes().strip_prefix(b"Bearer "),
            Some(token.as_bytes())
        );
    }

    // The right token in the right scheme is always accepted.
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert(AUTHORIZATION, v);
        assert!(bearer.check(&headers).is_ok());
    }
});
