//! Automatic certificates over ACME (RFC 8555), on rustls, hyper and ring.
//!
//! - [`client`]: the protocol: the account, orders, authorizations,
//!   finalization with a CSR and the certificate download, every request
//!   a JWS signed with the account's P-256 key, and the account kept on
//!   disk at mode 0600;
//! - [`challenges`]: proving control of a name: an HTTP-01 responder, a
//!   TLS-ALPN-01 certificate the TLS listener serves to the CA, and a
//!   DNS-01 command hook.

pub mod challenges;
pub mod client;

/// Base64url without padding (RFC 4648 section 5), as JWS uses it.
pub fn b64url(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// PEM with the given label, 64 columns, as Go and OpenSSL write it.
pub fn pem(label: &str, der: &[u8]) -> String {
    let b64 = {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(der.len().div_ceil(3) * 4);
        for chunk in der.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    };
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_matches_rfc4648() {
        for (input, want) in [
            (&b""[..], ""),
            (b"f", "Zg"),
            (b"fo", "Zm8"),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg"),
            (b"fooba", "Zm9vYmE"),
            (b"foobar", "Zm9vYmFy"),
            (&[0xfb, 0xff], "-_8"),
        ] {
            assert_eq!(b64url(input), want);
        }
    }

    #[test]
    fn pem_wraps_at_64() {
        let p = pem("CERTIFICATE", &[0u8; 60]);
        let lines: Vec<&str> = p.lines().collect();
        assert_eq!(lines[0], "-----BEGIN CERTIFICATE-----");
        assert_eq!(lines[1].len(), 64);
        assert_eq!(lines[2], "AAAAAAAAAAAAAAAA");
        assert_eq!(lines[3], "-----END CERTIFICATE-----");
        assert!(pem("X", b"fo").contains("Zm8=\n"));
        assert!(pem("X", b"f").contains("Zg==\n"));
    }
}
