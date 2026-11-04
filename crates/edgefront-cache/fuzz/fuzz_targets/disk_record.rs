#![no_main]

use edgefront_cache::store::codec::{MAGIC, decode, decode_header, encode};
use libfuzzer_sys::fuzz_target;
use sha2::{Digest, Sha256};

/// Wraps fuzz bytes in a record with valid checksums, so the header parser
/// is exercised instead of the checksum check rejecting nearly everything.
fn record(header: &[u8]) -> Option<Vec<u8>> {
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&u32::try_from(header.len()).ok()?.to_be_bytes());
    out.extend_from_slice(header);
    let sum = Sha256::digest(&out);
    out.extend_from_slice(&sum);
    let parsed = decode_header(&out).ok()?;
    let body_len: usize = parsed.variants.iter().map(|v| v.body_len).sum();
    if body_len > 1 << 20 {
        return None;
    }
    let bodies = vec![b'b'; body_len];
    out.extend_from_slice(&bodies);
    out.extend_from_slice(&Sha256::digest(&bodies));
    Some(out)
}

fuzz_target!(|data: &[u8]| {
    // Raw bytes: must never panic.
    let _ = decode(data);
    let Some(rec) = record(data) else { return };
    let (key, entry) = decode(&rec).expect("a record with a valid header decodes");
    // What decodes survives a re-encode and decode unchanged.
    let again = encode(&key, &entry).expect("decoded entries re-encode");
    let (key2, entry2) = decode(&again).expect("re-encoded record decodes");
    assert_eq!(key, key2);
    assert_eq!(entry.variants.len(), entry2.variants.len());
    for (a, b) in entry.variants.iter().zip(&entry2.variants) {
        assert_eq!(
            (a.status, &a.headers, &a.body, a.vary()),
            (b.status, &b.headers, &b.body, b.vary())
        );
    }
});
