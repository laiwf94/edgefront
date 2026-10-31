//! Byte-range requests (RFC 9110 section 14).
//!
//! Only single ranges are served. A request for several ranges gets the
//! whole object with 200, which RFC 9110 section 14.2 permits; it also
//! avoids the multipart amplification some range-overlap attacks rely on.

/// How to answer a Range header for an object of a known length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRange {
    /// Serve the whole object: no usable Range header.
    Ignore,
    /// Serve bytes `start..=end`.
    Satisfiable { start: u64, end: u64 },
    /// Answer 416.
    Unsatisfiable,
}

enum Spec {
    FromTo(u64, u64),
    From(u64),
    Suffix(u64),
}

/// Interprets a Range header value against an object of `len` bytes.
pub fn parse(value: &[u8], len: u64) -> ByteRange {
    let value = value.trim_ascii();
    let Some(eq) = value.iter().position(|&b| b == b'=') else {
        return ByteRange::Ignore;
    };
    if !value[..eq].trim_ascii().eq_ignore_ascii_case(b"bytes") {
        return ByteRange::Ignore;
    }
    let mut specs = Vec::new();
    for part in value[eq + 1..].split(|&b| b == b',') {
        let part = part.trim_ascii();
        if part.is_empty() {
            continue;
        }
        match parse_spec(part) {
            Some(spec) => specs.push(spec),
            None => return ByteRange::Ignore,
        }
        if specs.len() > 1 {
            return ByteRange::Ignore;
        }
    }
    let Some(spec) = specs.pop() else {
        return ByteRange::Ignore;
    };
    match spec {
        Spec::FromTo(start, _) | Spec::From(start) if start >= len => ByteRange::Unsatisfiable,
        Spec::FromTo(start, end) => ByteRange::Satisfiable {
            start,
            end: end.min(len - 1),
        },
        Spec::From(start) => ByteRange::Satisfiable {
            start,
            end: len - 1,
        },
        Spec::Suffix(0) => ByteRange::Unsatisfiable,
        Spec::Suffix(_) if len == 0 => ByteRange::Unsatisfiable,
        Spec::Suffix(n) => ByteRange::Satisfiable {
            start: len.saturating_sub(n),
            end: len - 1,
        },
    }
}

fn parse_spec(part: &[u8]) -> Option<Spec> {
    let dash = part.iter().position(|&b| b == b'-')?;
    let (first, last) = (part[..dash].trim_ascii(), part[dash + 1..].trim_ascii());
    match (first.is_empty(), last.is_empty()) {
        (true, true) => None,
        (true, false) => Some(Spec::Suffix(number(last)?)),
        (false, true) => Some(Spec::From(number(first)?)),
        (false, false) => {
            let (a, b) = (number(first)?, number(last)?);
            (a <= b).then_some(Spec::FromTo(a, b))
        }
    }
}

/// Parses ASCII digits, saturating instead of overflowing.
fn number(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(digits.iter().fold(0u64, |n, d| {
        n.saturating_mul(10).saturating_add(u64::from(d - b'0'))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(v: &str, len: u64) -> ByteRange {
        parse(v.as_bytes(), len)
    }

    fn sat(start: u64, end: u64) -> ByteRange {
        ByteRange::Satisfiable { start, end }
    }

    #[test]
    fn simple_ranges() {
        assert_eq!(r("bytes=0-99", 1000), sat(0, 99));
        assert_eq!(r("bytes=500-", 1000), sat(500, 999));
        assert_eq!(r("bytes=-100", 1000), sat(900, 999));
        assert_eq!(r("bytes=0-0", 1), sat(0, 0));
        assert_eq!(r("Bytes = 1-2 ", 10), sat(1, 2));
        assert_eq!(r("bytes=,0-1,", 10), sat(0, 1));
    }

    #[test]
    fn ranges_past_the_end_are_clamped_or_refused() {
        assert_eq!(r("bytes=0-5000", 1000), sat(0, 999));
        assert_eq!(r("bytes=-5000", 1000), sat(0, 999));
        assert_eq!(r("bytes=1000-", 1000), ByteRange::Unsatisfiable);
        assert_eq!(r("bytes=1000-2000", 1000), ByteRange::Unsatisfiable);
        assert_eq!(r("bytes=-0", 1000), ByteRange::Unsatisfiable);
    }

    #[test]
    fn empty_objects_satisfy_nothing() {
        assert_eq!(r("bytes=0-", 0), ByteRange::Unsatisfiable);
        assert_eq!(r("bytes=-5", 0), ByteRange::Unsatisfiable);
    }

    #[test]
    fn invalid_or_multiple_ranges_are_ignored() {
        for v in [
            "",
            "bytes",
            "items=0-1",
            "bytes=",
            "bytes=-",
            "bytes=a-b",
            "bytes=5-1",
            "bytes=0-1,5-6",
            "bytes=1",
            "bytes=0-1;x",
        ] {
            assert_eq!(r(v, 100), ByteRange::Ignore, "{v:?}");
        }
    }

    #[test]
    fn huge_numbers_saturate() {
        assert_eq!(r("bytes=0-99999999999999999999999", 10), sat(0, 9));
        assert_eq!(
            r("bytes=99999999999999999999999-", 10),
            ByteRange::Unsatisfiable
        );
    }
}
