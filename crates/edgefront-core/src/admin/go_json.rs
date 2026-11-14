//! JSON as Go's `encoding/json` writes it, so steer's admin documents come
//! out byte for byte the same.
//!
//! Go writes struct fields in declaration order and map keys sorted, so a
//! [`Json::Object`] keeps the order it is built in and [`Json::map`] sorts.
//! Strings escape `<`, `>`, `&`, U+2028 and U+2029 as well as what JSON
//! requires, and floats take the shortest form that reads back the same,
//! in exponent form below 1e-6 and from 1e21 up. [`Json::indented`] is
//! `json.Encoder` with `SetIndent("", "  ")`: empty arrays and objects stay
//! `[]` and `{}`, and the document ends with a newline.

use std::fmt::Write as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A JSON value in the shape Go would encode it.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    /// Members in the order they are written: a Go struct's field order.
    Object(Vec<(String, Json)>),
}

/// A value Go's encoder refuses: a NaN or infinite float. Go writes
/// nothing at all for the document then.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Unsupported(pub f64);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "json: unsupported value: {}", go_float_text(self.0))
    }
}

impl std::error::Error for Unsupported {}

impl Json {
    /// A string value.
    pub fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }

    /// A Go struct: members in the order given.
    pub fn object<K: Into<String>>(members: impl IntoIterator<Item = (K, Json)>) -> Json {
        Json::Object(members.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// A Go map: members sorted by key, as Go writes them. A later
    /// duplicate key replaces an earlier one, as in a map.
    pub fn map<K: Into<String>>(members: impl IntoIterator<Item = (K, Json)>) -> Json {
        let mut sorted: Vec<(String, Json)> = Vec::new();
        for (k, v) in members {
            let k = k.into();
            match sorted.binary_search_by(|(have, _)| have.as_str().cmp(&k)) {
                Ok(i) => sorted[i].1 = v,
                Err(i) => sorted.insert(i, (k, v)),
            }
        }
        Json::Object(sorted)
    }

    /// A Go slice: `null` when it is nil, which is how Go writes a slice
    /// nothing was appended to. Pass `nil = false` for a slice made with
    /// `make`, which is `[]` when empty.
    pub fn slice(items: Vec<Json>, nil_when_empty: bool) -> Json {
        if items.is_empty() && nil_when_empty {
            Json::Null
        } else {
            Json::Array(items)
        }
    }

    /// A `time.Time` as Go marshals it: RFC 3339 with nanoseconds, trailing
    /// zeros dropped, in UTC.
    pub fn time(t: SystemTime) -> Json {
        Json::Str(rfc3339_nano(t))
    }

    /// The compact encoding, as `json.Marshal` writes it.
    pub fn compact(&self) -> Result<String, Unsupported> {
        let mut out = String::new();
        self.write(&mut out, None, 0)?;
        Ok(out)
    }

    /// The document as steer's admin API wrote it: `json.Encoder` with a
    /// two-space indent, ending with a newline.
    pub fn indented(&self) -> Result<String, Unsupported> {
        let mut out = String::new();
        self.write(&mut out, Some("  "), 0)?;
        out.push('\n');
        Ok(out)
    }

    fn write(
        &self,
        out: &mut String,
        indent: Option<&str>,
        depth: usize,
    ) -> Result<(), Unsupported> {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Json::Uint(n) => {
                let _ = write!(out, "{n}");
            }
            Json::Float(f) => {
                if !f.is_finite() {
                    return Err(Unsupported(*f));
                }
                out.push_str(&go_float(*f));
            }
            Json::Str(s) => push_go_string(out, s),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline(out, indent, depth + 1);
                    item.write(out, indent, depth + 1)?;
                }
                if !items.is_empty() {
                    newline(out, indent, depth);
                }
                out.push(']');
            }
            Json::Object(members) => {
                out.push('{');
                for (i, (k, v)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline(out, indent, depth + 1);
                    push_go_string(out, k);
                    out.push(':');
                    if indent.is_some() {
                        out.push(' ');
                    }
                    v.write(out, indent, depth + 1)?;
                }
                if !members.is_empty() {
                    newline(out, indent, depth);
                }
                out.push('}');
            }
        }
        Ok(())
    }
}

fn newline(out: &mut String, indent: Option<&str>, depth: usize) {
    if let Some(unit) = indent {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(unit);
        }
    }
}

/// A string as Go's `encoding/json` quotes it, with HTML escaping on.
pub fn go_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    push_go_string(&mut out, s);
    out
}

fn push_go_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A finite float64 as Go's `encoding/json` writes it: the shortest
/// decimal that reads back the same, in exponent form below 1e-6 and from
/// 1e21 up, with the exponent's sign and no leading zero in it.
pub fn go_float(f: f64) -> String {
    let abs = f.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        let e = format!("{f:e}");
        match e.split_once('e') {
            Some((m, x)) if !x.starts_with('-') => format!("{m}e+{x}"),
            _ => e,
        }
    } else {
        format!("{f}")
    }
}

/// How Go's `%v` names a float the encoder refused.
fn go_float_text(f: f64) -> &'static str {
    if f.is_nan() {
        "NaN"
    } else if f > 0.0 {
        "+Inf"
    } else {
        "-Inf"
    }
}

/// `t` in Go's `time.RFC3339Nano` layout, in UTC: trailing zeros of the
/// fraction dropped, and no fraction at all on a whole second.
pub fn rfc3339_nano(t: SystemTime) -> String {
    // Nanoseconds from the epoch, negative before it.
    let nanos: i128 = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    };
    let secs = nanos.div_euclid(1_000_000_000);
    let frac = nanos.rem_euclid(1_000_000_000) as u32;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    let mut out = format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
        sod / 3600,
        sod / 60 % 60,
        sod % 60
    );
    if frac != 0 {
        let digits = format!("{frac:09}");
        out.push('.');
        out.push_str(digits.trim_end_matches('0'));
    }
    out.push('Z');
    out
}

/// The proleptic Gregorian date of a day count from 1970-01-01 (Howard
/// Hinnant's `civil_from_days`).
fn civil_from_days(z: i128) -> (i128, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `UNIX_EPOCH + secs + nanos`, for building times in tests and fixtures.
pub fn unix(secs: u64, nanos: u32) -> SystemTime {
    UNIX_EPOCH + Duration::new(secs, nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_and_floats() {
        assert_eq!(go_string("a<b>&c"), r#""a\u003cb\u003e\u0026c""#);
        assert_eq!(
            go_string("\"\\\n\r\t\u{8}\u{c}\u{1}\u{2028}\u{2029}é"),
            r#""\"\\\n\r\t\b\f\u0001\u2028\u2029é""#
        );
        assert_eq!(go_float(0.0), "0");
        assert_eq!(go_float(-0.0), "-0");
        assert_eq!(go_float(1.0), "1");
        assert_eq!(go_float(0.5), "0.5");
        assert_eq!(go_float(0.000001), "0.000001");
        assert_eq!(go_float(1.5e-7), "1.5e-7");
        assert_eq!(go_float(1e21), "1e+21");
        assert_eq!(go_float(1e20), "100000000000000000000");
        assert_eq!(go_float(-2.5e300), "-2.5e+300");
        assert_eq!(go_float(-2.5e-9), "-2.5e-9");
    }

    #[test]
    fn indentation_follows_go() {
        let v = Json::object([
            ("a", Json::Int(-1)),
            ("b", Json::Array(vec![Json::Uint(1), Json::Bool(false)])),
            ("c", Json::Array(vec![])),
            ("d", Json::Object(vec![])),
            ("e", Json::map([("z", Json::Null), ("y", Json::str("<"))])),
        ]);
        assert_eq!(
            v.indented().unwrap(),
            "{\n  \"a\": -1,\n  \"b\": [\n    1,\n    false\n  ],\n  \"c\": [],\n  \"d\": {},\n  \"e\": {\n    \"y\": \"\\u003c\",\n    \"z\": null\n  }\n}\n"
        );
        assert_eq!(
            v.compact().unwrap(),
            r#"{"a":-1,"b":[1,false],"c":[],"d":{},"e":{"y":"\u003c","z":null}}"#
        );
        assert_eq!(Json::Float(0.25).indented().unwrap(), "0.25\n");
    }

    #[test]
    fn maps_sort_and_replace() {
        let m = Json::map([
            ("b", Json::Int(1)),
            ("a", Json::Int(2)),
            ("b", Json::Int(3)),
        ]);
        assert_eq!(m.compact().unwrap(), r#"{"a":2,"b":3}"#);
    }

    #[test]
    fn nil_slices() {
        assert_eq!(Json::slice(vec![], true), Json::Null);
        assert_eq!(Json::slice(vec![], false), Json::Array(vec![]));
        assert_eq!(
            Json::slice(vec![Json::Int(1)], true),
            Json::Array(vec![Json::Int(1)])
        );
    }

    #[test]
    fn unsupported_floats() {
        for (f, text) in [
            (f64::NAN, "NaN"),
            (f64::INFINITY, "+Inf"),
            (f64::NEG_INFINITY, "-Inf"),
        ] {
            let err = Json::Array(vec![Json::Float(f)]).indented().unwrap_err();
            assert_eq!(err.to_string(), format!("json: unsupported value: {text}"));
        }
        let nested = Json::object([("x", Json::Float(f64::NAN))]);
        assert!(nested.compact().is_err());
    }

    #[test]
    fn times() {
        assert_eq!(rfc3339_nano(unix(0, 0)), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339_nano(unix(1_794_704_523, 450_000_000)),
            "2026-11-15T01:02:03.45Z"
        );
        assert_eq!(
            rfc3339_nano(unix(951_782_400, 1)),
            "2000-02-29T00:00:00.000000001Z"
        );
        assert_eq!(rfc3339_nano(unix(4_107_542_400, 0)), "2100-03-01T00:00:00Z");
        assert_eq!(
            rfc3339_nano(UNIX_EPOCH - Duration::new(1, 500_000_000)),
            "1969-12-31T23:59:58.5Z"
        );
        assert_eq!(
            Json::time(unix(86_399, 999_999_999)),
            Json::str("1970-01-01T23:59:59.999999999Z")
        );
    }
}
