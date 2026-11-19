//! Just enough DER to read what the TLS edge needs from an X.509
//! certificate (its validity period, subject alternative names, OCSP
//! responder, key usages, and the parts an OCSP request or a signature
//! check is built from), and to write the few structures it sends: OCSP
//! requests and ACME certificate signing requests.
//!
//! rustls checks chains and names itself but does not expose a
//! certificate's fields, and the store needs them to build its SNI table
//! from the certificate's own DNS names and to warn about certificates
//! outside their validity period. Parsing is strict DER (definite lengths
//! in their shortest form, no trailing bytes), so a malformed certificate
//! is refused at load rather than half read.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Tags this module reads and writes.
pub mod tag {
    pub const BOOLEAN: u8 = 0x01;
    pub const INTEGER: u8 = 0x02;
    pub const BIT_STRING: u8 = 0x03;
    pub const OCTET_STRING: u8 = 0x04;
    pub const NULL: u8 = 0x05;
    pub const OID: u8 = 0x06;
    pub const ENUMERATED: u8 = 0x0a;
    pub const UTF8_STRING: u8 = 0x0c;
    pub const SET: u8 = 0x31;
    pub const UTC_TIME: u8 = 0x17;
    pub const GENERALIZED_TIME: u8 = 0x18;
    pub const SEQUENCE: u8 = 0x30;
    /// `[n]` constructed, as in an explicit context tag.
    pub const fn context(n: u8) -> u8 {
        0xa0 | n
    }
    /// `[n]` primitive, as in an implicit context tag on a string.
    pub const fn context_primitive(n: u8) -> u8 {
        0x80 | n
    }
}

/// A malformed encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("malformed DER")
    }
}

impl std::error::Error for Malformed {}

/// Reads DER values one after another from a byte string.
#[derive(Debug, Clone, Copy)]
pub struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Reader { rest: bytes }
    }

    pub fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }

    /// The tag of the next value, if there is one.
    pub fn peek(&self) -> Option<u8> {
        self.rest.first().copied()
    }

    /// Reads the next value: its tag, its contents, and the whole
    /// encoding (tag, length and contents).
    pub fn any(&mut self) -> Result<(u8, &'a [u8], &'a [u8]), Malformed> {
        let all = self.rest;
        let (&tag, rest) = all.split_first().ok_or(Malformed)?;
        // High tag numbers never occur in what this module reads.
        if tag & 0x1f == 0x1f {
            return Err(Malformed);
        }
        let (&first, mut rest) = rest.split_first().ok_or(Malformed)?;
        let len = if first < 0x80 {
            usize::from(first)
        } else {
            let n = usize::from(first & 0x7f);
            if n == 0 || n > 4 || rest.len() < n {
                return Err(Malformed);
            }
            let (bytes, after) = rest.split_at(n);
            rest = after;
            if bytes[0] == 0 {
                return Err(Malformed);
            }
            let len = bytes.iter().fold(0usize, |l, &b| (l << 8) | usize::from(b));
            if len < 0x80 {
                return Err(Malformed);
            }
            len
        };
        if rest.len() < len {
            return Err(Malformed);
        }
        let (contents, after) = rest.split_at(len);
        let header = all.len() - rest.len();
        self.rest = after;
        Ok((tag, contents, &all[..header + len]))
    }

    /// Reads the next value, which must have tag `want`, and returns its
    /// contents.
    pub fn read(&mut self, want: u8) -> Result<&'a [u8], Malformed> {
        let (tag, contents, _) = self.any()?;
        if tag != want {
            return Err(Malformed);
        }
        Ok(contents)
    }

    /// Reads a value with tag `want` if it comes next.
    pub fn optional(&mut self, want: u8) -> Result<Option<&'a [u8]>, Malformed> {
        if self.peek() == Some(want) {
            self.read(want).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Reads a SEQUENCE and returns a reader over its contents.
    pub fn sequence(&mut self) -> Result<Reader<'a>, Malformed> {
        self.read(tag::SEQUENCE).map(Reader::new)
    }

    /// Fails unless everything has been read.
    pub fn finish(&self) -> Result<(), Malformed> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(Malformed)
        }
    }
}

/// What the TLS edge reads from a certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertFields {
    pub not_before: SystemTime,
    pub not_after: SystemTime,
    /// The DNS names in the subject alternative names, as written.
    pub dns_names: Vec<String>,
    /// The URIs in the subject alternative names, as written.
    pub uris: Vec<String>,
}

/// The DER of `2.5.29.17`, subjectAltName.
const OID_SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x11];

/// The parts of a certificate.
#[derive(Debug, Clone)]
pub struct Tbs<'a> {
    /// The whole tbsCertificate, tag and length included: what the
    /// signature covers.
    pub signed: &'a [u8],
    /// The signature algorithm's OID.
    pub signature_algorithm: &'a [u8],
    /// The signature, without the BIT STRING's unused-bits byte.
    pub signature: &'a [u8],
    /// The serial number's contents.
    pub serial: &'a [u8],
    /// The issuer Name, tag and length included.
    pub issuer: &'a [u8],
    /// The subject Name, tag and length included.
    pub subject: &'a [u8],
    /// The SubjectPublicKeyInfo, tag and length included.
    pub spki: &'a [u8],
    /// The subjectPublicKey bits, without the unused-bits byte: what ring
    /// verifies with, and what an OCSP issuer key hash covers.
    pub public_key: &'a [u8],
    /// The public key algorithm's OID.
    pub key_algorithm: &'a [u8],
    pub validity: Reader<'a>,
    /// The extensions as (OID, critical, value) triples.
    pub extensions: Vec<(&'a [u8], bool, &'a [u8])>,
}

/// A BIT STRING's bits, which must be whole bytes.
pub fn bits(contents: &[u8]) -> Result<&[u8], Malformed> {
    match contents.split_first() {
        Some((0, rest)) => Ok(rest),
        _ => Err(Malformed),
    }
}

/// Splits a certificate into its parts.
pub fn tbs(cert: &[u8]) -> Result<Tbs<'_>, Malformed> {
    let mut outer = Reader::new(cert);
    let mut certificate = outer.sequence()?;
    outer.finish()?;
    let (t, contents, signed) = certificate.any()?;
    if t != tag::SEQUENCE {
        return Err(Malformed);
    }
    let mut tbs = Reader::new(contents);
    let mut alg = certificate.sequence()?;
    let signature_algorithm = alg.read(tag::OID)?;
    let signature = bits(certificate.read(tag::BIT_STRING)?)?;
    certificate.finish()?;

    tbs.optional(tag::context(0))?; // version
    let serial = tbs.read(tag::INTEGER)?;
    tbs.read(tag::SEQUENCE)?; // signature
    let issuer = whole(&mut tbs, tag::SEQUENCE)?;
    let validity = tbs.sequence()?;
    let subject = whole(&mut tbs, tag::SEQUENCE)?;
    let spki = whole(&mut tbs, tag::SEQUENCE)?;
    let mut info = Reader::new(spki).sequence()?;
    let key_algorithm = info.sequence()?.read(tag::OID)?;
    let public_key = bits(info.read(tag::BIT_STRING)?)?;
    tbs.optional(tag::context_primitive(1))?; // issuerUniqueID
    tbs.optional(tag::context_primitive(2))?; // subjectUniqueID
    let mut extensions = Vec::new();
    if let Some(exts) = tbs.optional(tag::context(3))? {
        let mut exts = Reader::new(exts);
        let mut list = exts.sequence()?;
        exts.finish()?;
        while !list.is_empty() {
            let mut ext = list.sequence()?;
            let oid = ext.read(tag::OID)?;
            let critical = match ext.optional(tag::BOOLEAN)? {
                Some([0xff]) => true,
                Some([0x00]) => false,
                Some(_) => return Err(Malformed),
                None => false,
            };
            let value = ext.read(tag::OCTET_STRING)?;
            ext.finish()?;
            extensions.push((oid, critical, value));
        }
    }
    tbs.finish()?;
    Ok(Tbs {
        signed,
        signature_algorithm,
        signature,
        serial,
        issuer,
        subject,
        spki,
        public_key,
        key_algorithm,
        validity,
        extensions,
    })
}

/// Reads a value with tag `want` and returns its whole encoding.
pub fn whole<'a>(r: &mut Reader<'a>, want: u8) -> Result<&'a [u8], Malformed> {
    let (t, _, all) = r.any()?;
    if t != want {
        return Err(Malformed);
    }
    Ok(all)
}

/// The DER of `1.3.6.1.5.5.7.1.1`, authorityInfoAccess.
const OID_AUTHORITY_INFO_ACCESS: &[u8] = &[0x2b, 6, 1, 5, 5, 7, 1, 1];
/// The DER of `1.3.6.1.5.5.7.48.1`, id-ad-ocsp.
const OID_AD_OCSP: &[u8] = &[0x2b, 6, 1, 5, 5, 7, 48, 1];
/// The DER of `2.5.29.37`, extKeyUsage.
const OID_EXT_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x25];
/// The DER of `1.3.6.1.5.5.7.3.9`, id-kp-OCSPSigning.
pub const OID_KP_OCSP_SIGNING: &[u8] = &[0x2b, 6, 1, 5, 5, 7, 3, 9];

impl Tbs<'_> {
    /// The OCSP responder URLs from authorityInfoAccess.
    pub fn ocsp_urls(&self) -> Result<Vec<String>, Malformed> {
        let mut out = Vec::new();
        for (oid, _, value) in &self.extensions {
            if *oid != OID_AUTHORITY_INFO_ACCESS {
                continue;
            }
            let mut outer = Reader::new(value);
            let mut list = outer.sequence()?;
            outer.finish()?;
            while !list.is_empty() {
                let mut ad = list.sequence()?;
                let method = ad.read(tag::OID)?;
                let (t, name, _) = ad.any()?;
                if method == OID_AD_OCSP && t == tag::context_primitive(6) {
                    out.push(std::str::from_utf8(name).map_err(|_| Malformed)?.to_owned());
                }
            }
        }
        Ok(out)
    }

    /// The extended key usage OIDs, if the extension is present.
    pub fn extended_key_usage(&self) -> Result<Option<Vec<&[u8]>>, Malformed> {
        for (oid, _, value) in &self.extensions {
            if *oid != OID_EXT_KEY_USAGE {
                continue;
            }
            let mut outer = Reader::new(value);
            let mut list = outer.sequence()?;
            outer.finish()?;
            let mut out = Vec::new();
            while !list.is_empty() {
                out.push(list.read(tag::OID)?);
            }
            return Ok(Some(out));
        }
        Ok(None)
    }
}

/// Writing DER: each function returns one whole value.
pub mod write {
    use super::tag;
    use std::time::SystemTime;

    /// A value with `tag` around `contents`.
    pub fn tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(contents.len() + 6);
        out.push(tag);
        let n = contents.len();
        if n < 0x80 {
            out.push(n as u8);
        } else {
            let bytes = n.to_be_bytes();
            let skip = bytes.iter().take_while(|&&b| b == 0).count();
            out.push(0x80 | (bytes.len() - skip) as u8);
            out.extend_from_slice(&bytes[skip..]);
        }
        out.extend_from_slice(contents);
        out
    }

    /// A SEQUENCE of already encoded values.
    pub fn sequence(parts: &[&[u8]]) -> Vec<u8> {
        tlv(tag::SEQUENCE, &parts.concat())
    }

    /// A SET of already encoded values, sorted as DER requires.
    pub fn set(parts: &[&[u8]]) -> Vec<u8> {
        let mut sorted = parts.to_vec();
        sorted.sort();
        tlv(tag::SET, &sorted.concat())
    }

    /// An INTEGER from big-endian unsigned bytes.
    pub fn unsigned(be: &[u8]) -> Vec<u8> {
        let trimmed = match be.iter().position(|&b| b != 0) {
            Some(i) => &be[i..],
            None => &[0u8][..],
        };
        let mut c = Vec::with_capacity(trimmed.len() + 1);
        if trimmed[0] & 0x80 != 0 {
            c.push(0);
        }
        c.extend_from_slice(trimmed);
        tlv(tag::INTEGER, &c)
    }

    /// An INTEGER whose contents are already DER (a serial number read
    /// from a certificate).
    pub fn integer_raw(contents: &[u8]) -> Vec<u8> {
        tlv(tag::INTEGER, contents)
    }

    /// An OBJECT IDENTIFIER from its encoded contents.
    pub fn oid(contents: &[u8]) -> Vec<u8> {
        tlv(tag::OID, contents)
    }

    pub fn octet_string(b: &[u8]) -> Vec<u8> {
        tlv(tag::OCTET_STRING, b)
    }

    /// A BIT STRING of whole bytes.
    pub fn bit_string(b: &[u8]) -> Vec<u8> {
        let mut c = Vec::with_capacity(b.len() + 1);
        c.push(0);
        c.extend_from_slice(b);
        tlv(tag::BIT_STRING, &c)
    }

    pub fn null() -> Vec<u8> {
        vec![tag::NULL, 0]
    }

    pub fn boolean(b: bool) -> Vec<u8> {
        vec![tag::BOOLEAN, 1, if b { 0xff } else { 0 }]
    }

    pub fn enumerated(n: u8) -> Vec<u8> {
        tlv(tag::ENUMERATED, &[n])
    }

    pub fn utf8(s: &str) -> Vec<u8> {
        tlv(tag::UTF8_STRING, s.as_bytes())
    }

    /// `[n]` EXPLICIT around an encoded value.
    pub fn explicit(n: u8, inner: &[u8]) -> Vec<u8> {
        tlv(tag::context(n), inner)
    }

    /// `[n]` IMPLICIT on a primitive's contents.
    pub fn implicit(n: u8, contents: &[u8]) -> Vec<u8> {
        tlv(tag::context_primitive(n), contents)
    }

    /// A GeneralizedTime, `YYYYMMDDHHMMSSZ`.
    pub fn generalized_time(t: SystemTime) -> Vec<u8> {
        let secs = match t.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        };
        let (y, m, d) = super::civil_from_days(secs.div_euclid(86_400));
        let rem = secs.rem_euclid(86_400);
        let text = format!(
            "{y:04}{m:02}{d:02}{:02}{:02}{:02}Z",
            rem / 3600,
            rem % 3600 / 60,
            rem % 60
        );
        tlv(tag::GENERALIZED_TIME, text.as_bytes())
    }
}

/// The date of a day count from 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// Reads the validity period and subject alternative names of a DER
/// certificate.
pub fn cert_fields(cert: &[u8]) -> Result<CertFields, Malformed> {
    let t = tbs(cert)?;
    let mut validity = t.validity;
    let not_before = time(&mut validity)?;
    let not_after = time(&mut validity)?;
    validity.finish()?;
    let mut dns_names = Vec::new();
    let mut uris = Vec::new();
    for (oid, _, value) in &t.extensions {
        if *oid != OID_SUBJECT_ALT_NAME {
            continue;
        }
        let mut outer = Reader::new(value);
        let mut names = outer.sequence()?;
        outer.finish()?;
        while !names.is_empty() {
            let (tag, contents, _) = names.any()?;
            let text = || {
                std::str::from_utf8(contents)
                    .ok()
                    .filter(|s| s.is_ascii())
                    .map(str::to_owned)
                    .ok_or(Malformed)
            };
            if tag == tag::context_primitive(2) {
                dns_names.push(text()?);
            } else if tag == tag::context_primitive(6) {
                uris.push(text()?);
            }
        }
    }
    Ok(CertFields {
        not_before,
        not_after,
        dns_names,
        uris,
    })
}

/// Reads a UTCTime or GeneralizedTime in the forms RFC 5280 allows:
/// `YYMMDDHHMMSSZ` and `YYYYMMDDHHMMSSZ`.
pub fn time(r: &mut Reader<'_>) -> Result<SystemTime, Malformed> {
    let (tag, contents, _) = r.any()?;
    let (year, rest) = match tag {
        tag::UTC_TIME if contents.len() == 13 => {
            let yy = digits(&contents[..2])?;
            // RFC 5280 4.1.2.5.1: 50 and above are 19xx.
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, &contents[2..])
        }
        tag::GENERALIZED_TIME if contents.len() == 15 => (digits(&contents[..4])?, &contents[4..]),
        _ => return Err(Malformed),
    };
    if rest[10] != b'Z' {
        return Err(Malformed);
    }
    let month = digits(&rest[0..2])?;
    let day = digits(&rest[2..4])?;
    let hour = digits(&rest[4..6])?;
    let minute = digits(&rest[6..8])?;
    let second = digits(&rest[8..10])?;
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(Malformed);
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + i64::from(hour * 3600 + minute * 60 + second);
    Ok(if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs.unsigned_abs())
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    })
}

fn digits(b: &[u8]) -> Result<u32, Malformed> {
    b.iter().try_fold(0u32, |n, &c| {
        if c.is_ascii_digit() {
            Ok(n * 10 + u32::from(c - b'0'))
        } else {
            Err(Malformed)
        }
    })
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        _ => 31,
    }
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
pub fn days_from_civil(year: u32, month: u32, day: u32) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn t(tag: u8, s: &str) -> Result<SystemTime, Malformed> {
        let mut enc = vec![tag, s.len() as u8];
        enc.extend_from_slice(s.as_bytes());
        time(&mut Reader::new(&enc))
    }

    #[test]
    fn times_in_both_forms() {
        assert_eq!(t(tag::UTC_TIME, "700101000000Z"), Ok(at(0)));
        assert_eq!(t(tag::UTC_TIME, "491231235959Z"), Ok(at(2_524_607_999)));
        let fifty = t(tag::UTC_TIME, "500101000000Z").unwrap();
        assert_eq!(
            UNIX_EPOCH.duration_since(fifty).unwrap().as_secs(),
            631_152_000
        );
        assert_eq!(
            t(tag::GENERALIZED_TIME, "21260101000000Z"),
            Ok(at(4_922_899_200))
        );
        assert_eq!(t(tag::UTC_TIME, "240229120000Z"), Ok(at(1_709_208_000)));
        let before = t(tag::GENERALIZED_TIME, "19691231235959Z").unwrap();
        assert_eq!(UNIX_EPOCH.duration_since(before).unwrap().as_secs(), 1);
        for (tg, bad) in [
            (tag::UTC_TIME, "700101000000"),
            (tag::UTC_TIME, "700101000000+"),
            (tag::UTC_TIME, "7001010000a0Z"),
            (tag::UTC_TIME, "701301000000Z"),
            (tag::UTC_TIME, "700230000000Z"),
            (tag::UTC_TIME, "230229000000Z"),
            (tag::UTC_TIME, "700101240000Z"),
            (tag::UTC_TIME, "700101006000Z"),
            (tag::UTC_TIME, "700101000060Z"),
            (tag::UTC_TIME, "700100000000Z"),
            (tag::GENERALIZED_TIME, "700101000000Z"),
            (tag::OCTET_STRING, "700101000000Z"),
        ] {
            assert_eq!(t(tg, bad), Err(Malformed), "{bad}");
        }
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(days_in_month(1900, 2), 28);
    }

    #[test]
    fn lengths_are_strict() {
        let ok = |b: &[u8]| Reader::new(b).any().map(|(t, c, _)| (t, c.len()));
        assert_eq!(ok(&[0x04, 0x00]), Ok((4, 0)));
        assert_eq!(ok(&[0x04, 0x01, 0xaa]), Ok((4, 1)));
        let mut long = vec![0x04, 0x81, 0x80];
        long.extend([0; 0x80]);
        assert_eq!(ok(&long), Ok((4, 0x80)));
        for bad in [
            &[][..],
            &[0x04],
            &[0x04, 0x02, 0x00],
            &[0x04, 0x80],
            &[0x04, 0x81, 0x05, 0, 0, 0, 0, 0],
            &[0x04, 0x82, 0x00, 0x80],
            &[0x04, 0x85, 1, 0, 0, 0, 0],
            &[0x1f, 0x01, 0x00],
        ] {
            assert_eq!(ok(bad), Err(Malformed), "{bad:?}");
        }
        let mut r = Reader::new(&[0x05, 0x00, 0x05, 0x00]);
        assert_eq!(r.read(0x05), Ok(&[][..]));
        assert!(r.finish().is_err());
        assert_eq!(r.read(tag::INTEGER), Err(Malformed));
        assert_eq!(r.optional(tag::INTEGER), Ok(None));
        assert_eq!(Malformed.to_string(), "malformed DER");
    }

    #[test]
    fn certificates_that_are_not() {
        assert_eq!(cert_fields(b"\x30\x00"), Err(Malformed));
        assert_eq!(cert_fields(b"not der"), Err(Malformed));
    }
}
