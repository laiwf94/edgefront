//! Byte-range requests (RFC 9110 section 14).
//!
//! A Range header with up to [`MAX_RANGES`] ranges is served: one range as
//! a single 206, several as `multipart/byteranges` (built in
//! [`crate::respond`]). Ranges that overlap, or lie closer together than
//! [`COALESCE_GAP`], are coalesced into one, which RFC 9110 section
//! 15.3.7.2 permits whatever the order they were asked in; otherwise the
//! parts keep the order the client asked for. Two caps keep a request from
//! costing more than the object (section 14.2 lets a server ignore "more
//! than two overlapping ranges, or a set of many small ranges"):
//!
//! - more than [`MAX_RANGES`] ranges, and
//! - ranges whose lengths, before coalescing, add up to more than twice
//!   the object,
//!
//! are both ignored, and the whole object is served with 200.

/// The most ranges one request may ask for.
pub const MAX_RANGES: usize = 16;

/// Ranges closer than this many bytes are merged: a part's own headers
/// cost about this much, so sending the gap is cheaper.
pub const COALESCE_GAP: u64 = 80;

/// How to answer a Range header for an object of a known length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ByteRange {
    /// Serve the whole object: no usable Range header.
    Ignore,
    /// Serve bytes `start..=end` as a single part.
    Satisfiable { start: u64, end: u64 },
    /// Serve these inclusive ranges as `multipart/byteranges`, in order:
    /// at least two, none overlapping.
    Multiple(Vec<(u64, u64)>),
    /// Answer 416.
    Unsatisfiable,
}

enum Spec {
    FromTo(u64, u64),
    From(u64),
    Suffix(u64),
}

impl Spec {
    /// The inclusive range this spec selects in `len` bytes, if any.
    fn resolve(&self, len: u64) -> Option<(u64, u64)> {
        match *self {
            Spec::FromTo(start, _) | Spec::From(start) if start >= len => None,
            Spec::FromTo(start, end) => Some((start, end.min(len - 1))),
            Spec::From(start) => Some((start, len - 1)),
            Spec::Suffix(0) => None,
            Spec::Suffix(_) if len == 0 => None,
            Spec::Suffix(n) => Some((len.saturating_sub(n), len - 1)),
        }
    }
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
        if specs.len() > MAX_RANGES {
            return ByteRange::Ignore;
        }
    }
    if specs.is_empty() {
        return ByteRange::Ignore;
    }
    let ranges: Vec<(u64, u64)> = specs.iter().filter_map(|s| s.resolve(len)).collect();
    if ranges.is_empty() {
        return ByteRange::Unsatisfiable;
    }
    let requested = ranges
        .iter()
        .fold(0u64, |sum, (a, b)| sum.saturating_add(b - a + 1));
    if ranges.len() > 1 && requested > len.saturating_mul(2) {
        return ByteRange::Ignore;
    }
    let ranges = coalesce(ranges);
    match ranges.as_slice() {
        [(start, end)] => ByteRange::Satisfiable {
            start: *start,
            end: *end,
        },
        _ => ByteRange::Multiple(ranges),
    }
}

/// Keeps the requested order unless two ranges overlap or nearly touch;
/// then sorts them and merges every such pair.
fn coalesce(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    let close = |a: (u64, u64), b: (u64, u64)| {
        let (first, second) = if a.0 <= b.0 { (a, b) } else { (b, a) };
        second.0 <= first.1.saturating_add(COALESCE_GAP)
    };
    let any_close = ranges
        .iter()
        .enumerate()
        .any(|(i, &a)| ranges[i + 1..].iter().any(|&b| close(a, b)));
    if !any_close {
        return ranges;
    }
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1.saturating_add(COALESCE_GAP) => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
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
            "bytes=0-1,5-x",
            "bytes=1",
            "bytes=0-1;x",
        ] {
            assert_eq!(r(v, 100), ByteRange::Ignore, "{v:?}");
        }
    }

    fn multi(ranges: &[(u64, u64)]) -> ByteRange {
        ByteRange::Multiple(ranges.to_vec())
    }

    #[test]
    fn several_ranges_keep_their_order() {
        assert_eq!(r("bytes=0-1,500-501", 1000), multi(&[(0, 1), (500, 501)]));
        assert_eq!(r("bytes=500-501, 0-1", 1000), multi(&[(500, 501), (0, 1)]));
        assert_eq!(r("bytes=0-0,-1", 1000), multi(&[(0, 0), (999, 999)]));
    }

    #[test]
    fn overlapping_or_close_ranges_coalesce() {
        // Overlapping, out of order: sorted and merged.
        assert_eq!(
            r("bytes=500-600,0-99,550-700", 1000),
            multi(&[(0, 99), (500, 700)])
        );
        // A gap under COALESCE_GAP is sent rather than split.
        assert_eq!(r("bytes=0-9,50-59", 1000), sat(0, 59));
        assert_eq!(r("bytes=0-9,89-99", 1000), sat(0, 99));
        assert_eq!(r("bytes=0-9,90-99", 1000), multi(&[(0, 9), (90, 99)]));
        // Everything merges into one range: a single part.
        assert_eq!(r("bytes=0-499,400-999", 1000), sat(0, 999));
    }

    #[test]
    fn unsatisfiable_members_are_dropped() {
        assert_eq!(r("bytes=0-1,2000-3000", 1000), sat(0, 1));
        assert_eq!(r("bytes=2000-,3000-4000", 1000), ByteRange::Unsatisfiable);
        assert_eq!(
            r("bytes=0-1,-0,500-501", 1000),
            multi(&[(0, 1), (500, 501)])
        );
    }

    #[test]
    fn too_many_or_too_much_is_ignored() {
        let sixteen: Vec<String> = (0..16)
            .map(|i| format!("{}-{}", i * 200, i * 200 + 9))
            .collect();
        let v = format!("bytes={}", sixteen.join(","));
        assert!(matches!(r(&v, 10_000), ByteRange::Multiple(ref m) if m.len() == 16));
        let v = format!("bytes={},9000-9009", sixteen.join(","));
        assert_eq!(r(&v, 10_000), ByteRange::Ignore);
        // Three whole copies of the object, overlapping: more than twice its
        // length, so ignored rather than merged.
        assert_eq!(r("bytes=0-,0-,0-", 1000), ByteRange::Ignore);
        assert_eq!(r("bytes=0-,0-", 1000), sat(0, 999));
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
