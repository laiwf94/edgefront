//! A parsed document before it is decoded into the config schema, and the
//! two readers that produce it.
//!
//! The JSON reader accepts exactly what Go's `encoding/json` decoder does,
//! because steer's acceptance is the reference: strict RFC 8259 syntax, but
//! invalid UTF-8 and unpaired surrogate escapes inside strings become
//! U+FFFD instead of failing. Object members keep their order and their
//! duplicates, since Go applies duplicate keys in turn.
//!
//! The TOML reader goes through serde and the `toml` crate. TOML has no
//! null and forbids duplicate keys, so it produces a subset of the trees
//! JSON can.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

/// Nesting deeper than this is rejected while reading. The schema is six
/// levels deep, and Go rejects any value nested deeper than its field
/// allows, so no accepted document comes near the limit; it only keeps the
/// recursive reader's stack small.
const MAX_DEPTH: usize = 64;

/// One value of a parsed document.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Null,
    Bool(bool),
    /// A number as written. Integers are decoded from the text, as Go
    /// does, so `1.0` is not an integer.
    Number(String),
    String(String),
    Array(Vec<Node>),
    /// Members in document order, duplicates included.
    Object(Vec<(String, Node)>),
}

impl Node {
    /// The JSON type name used in decode errors.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Node::Null => "null",
            Node::Bool(_) => "a boolean",
            Node::Number(_) => "a number",
            Node::String(_) => "a string",
            Node::Array(_) => "an array",
            Node::Object(_) => "an object",
        }
    }
}

/// Reads one JSON document followed only by whitespace.
pub(crate) fn parse_json(data: &[u8]) -> Result<Node, String> {
    let mut r = Reader { data, pos: 0 };
    r.skip_ws();
    if r.pos == data.len() {
        return Err("unexpected end of input".to_owned());
    }
    let node = r.value(0)?;
    r.skip_ws();
    if r.pos != data.len() {
        return Err(format!(
            "trailing data after the configuration object at {}",
            r.location(r.pos)
        ));
    }
    Ok(node)
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn skip_ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.data.get(self.pos) {
            self.pos += 1;
        }
    }

    /// "line L, column C" of a byte offset, both counted from 1.
    fn location(&self, pos: usize) -> String {
        let before = &self.data[..pos];
        let line = before.iter().filter(|&&c| c == b'\n').count() + 1;
        let col = pos
            - before
                .iter()
                .rposition(|&c| c == b'\n')
                .map_or(0, |i| i + 1)
            + 1;
        format!("line {line}, column {col}")
    }

    fn error(&self, what: &str) -> String {
        match self.data.get(self.pos) {
            None => format!("unexpected end of input {what}"),
            Some(&c) => {
                let shown = if c.is_ascii_graphic() || c == b' ' {
                    format!("'{}'", char::from(c))
                } else {
                    format!("byte 0x{c:02x}")
                };
                format!(
                    "invalid character {shown} {what} at {}",
                    self.location(self.pos)
                )
            }
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.data.get(self.pos) == Some(&c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Result<Node, String> {
        if depth >= MAX_DEPTH {
            return Err(format!(
                "nesting deeper than {MAX_DEPTH} levels at {}",
                self.location(self.pos)
            ));
        }
        match self.data.get(self.pos) {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Node::String),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b't') => self.literal("true", Node::Bool(true)),
            Some(b'f') => self.literal("false", Node::Bool(false)),
            Some(b'n') => self.literal("null", Node::Null),
            _ => Err(self.error("looking for beginning of value")),
        }
    }

    fn literal(&mut self, word: &str, node: Node) -> Result<Node, String> {
        for &c in word.as_bytes() {
            if !self.eat(c) {
                return Err(self.error(&format!("in literal {word}")));
            }
        }
        Ok(node)
    }

    fn object(&mut self, depth: usize) -> Result<Node, String> {
        self.pos += 1;
        let mut members = Vec::new();
        self.skip_ws();
        if self.eat(b'}') {
            return Ok(Node::Object(members));
        }
        loop {
            self.skip_ws();
            if self.data.get(self.pos) != Some(&b'"') {
                return Err(self.error("looking for beginning of object key string"));
            }
            let key = self.string()?;
            self.skip_ws();
            if !self.eat(b':') {
                return Err(self.error("after object key"));
            }
            self.skip_ws();
            let value = self.value(depth + 1)?;
            members.push((key, value));
            self.skip_ws();
            if self.eat(b'}') {
                return Ok(Node::Object(members));
            }
            if !self.eat(b',') {
                return Err(self.error("after object key:value pair"));
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Node, String> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.eat(b']') {
            return Ok(Node::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            if self.eat(b']') {
                return Ok(Node::Array(items));
            }
            if !self.eat(b',') {
                return Err(self.error("after array element"));
            }
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while self.data.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn number(&mut self) -> Result<Node, String> {
        let start = self.pos;
        self.eat(b'-');
        if !self.eat(b'0') && self.digits() == 0 {
            return Err(self.error("in numeric literal"));
        }
        if self.eat(b'.') && self.digits() == 0 {
            return Err(self.error("after decimal point in numeric literal"));
        }
        if self.eat(b'e') || self.eat(b'E') {
            let _ = self.eat(b'+') || self.eat(b'-');
            if self.digits() == 0 {
                return Err(self.error("in exponent of numeric literal"));
            }
        }
        // Only ASCII was consumed.
        let text = String::from_utf8_lossy(&self.data[start..self.pos]).into_owned();
        Ok(Node::Number(text))
    }

    /// Four hex digits after `\u`, if they are there.
    fn hex4(&self, at: usize) -> Option<u32> {
        let digits = self.data.get(at..at + 4)?;
        let text = std::str::from_utf8(digits).ok()?;
        if !text.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(text, 16).ok()
    }

    fn string(&mut self) -> Result<String, String> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let Some(&c) = self.data.get(self.pos) else {
                return Err(self.error("in string literal"));
            };
            match c {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => self.escape(&mut out)?,
                0..0x20 => return Err(self.error("in string literal")),
                0x20..0x80 => {
                    out.push(char::from(c));
                    self.pos += 1;
                }
                _ => {
                    // Go decodes one rune at a time and replaces each byte
                    // that does not start a valid one with U+FFFD.
                    let width = match c {
                        0xC2..=0xDF => 2,
                        0xE0..=0xEF => 3,
                        0xF0..=0xF4 => 4,
                        _ => 0,
                    };
                    let valid = self
                        .data
                        .get(self.pos..self.pos + width)
                        .filter(|_| width > 0)
                        .and_then(|bytes| std::str::from_utf8(bytes).ok());
                    match valid {
                        Some(s) => {
                            out.push_str(s);
                            self.pos += width;
                        }
                        None => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            self.pos += 1;
                        }
                    }
                }
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), String> {
        self.pos += 1;
        let Some(&c) = self.data.get(self.pos) else {
            return Err(self.error("in string escape code"));
        };
        let simple = match c {
            b'"' => Some('"'),
            b'\\' => Some('\\'),
            b'/' => Some('/'),
            b'b' => Some('\u{8}'),
            b'f' => Some('\u{c}'),
            b'n' => Some('\n'),
            b'r' => Some('\r'),
            b't' => Some('\t'),
            b'u' => None,
            _ => return Err(self.error("in string escape code")),
        };
        if let Some(ch) = simple {
            out.push(ch);
            self.pos += 1;
            return Ok(());
        }
        let Some(unit) = self.hex4(self.pos + 1) else {
            self.pos += 1;
            while self.data.get(self.pos).is_some_and(u8::is_ascii_hexdigit) {
                self.pos += 1;
            }
            return Err(self.error("in \\u hexadecimal character escape"));
        };
        self.pos += 5;
        if (0xD800..0xE000).contains(&unit) {
            // A high surrogate followed by an escaped low one is a pair;
            // anything else is U+FFFD and what follows is read as usual.
            let low = (self.data.get(self.pos..self.pos + 2) == Some(b"\\u"))
                .then(|| self.hex4(self.pos + 2))
                .flatten();
            if let Some(low) = low
                && (0xD800..0xDC00).contains(&unit)
                && (0xDC00..0xE000).contains(&low)
            {
                let code = 0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
                out.push(char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER));
                self.pos += 6;
            } else {
                out.push(char::REPLACEMENT_CHARACTER);
            }
        } else {
            out.push(char::from_u32(unit).unwrap_or(char::REPLACEMENT_CHARACTER));
        }
        Ok(())
    }
}

/// Reads a TOML document.
pub(crate) fn parse_toml(data: &[u8]) -> Result<Node, String> {
    let text = std::str::from_utf8(data).map_err(|e| format!("not UTF-8: {e}"))?;
    toml::from_str::<Node>(text).map_err(|e| e.to_string().trim_end().to_owned())
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(NodeVisitor)
    }
}

struct NodeVisitor;

impl<'de> Visitor<'de> for NodeVisitor {
    type Value = Node;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a configuration value")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Node, E> {
        Ok(Node::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Node, E> {
        Ok(Node::Number(v.to_string()))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Node, E> {
        Ok(Node::Number(v.to_string()))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Node, E> {
        // Debug keeps a fraction or exponent ("1.0", "1e300"), so a float
        // never decodes as an integer.
        Ok(Node::Number(format!("{v:?}")))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Node, E> {
        Ok(Node::String(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Node, E> {
        Ok(Node::String(v))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Node, E> {
        Ok(Node::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Node, E> {
        Ok(Node::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Node, D::Error> {
        Node::deserialize(d)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Node, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Node::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Node, A::Error> {
        let mut members = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, Node>()? {
            members.push((key, value));
        }
        Ok(Node::Object(members))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Node {
        Node::String(v.to_owned())
    }

    fn n(v: &str) -> Node {
        Node::Number(v.to_owned())
    }

    #[test]
    fn reads_every_kind_of_value() {
        let doc =
            br#" {"a": [1, -0.5e+3, 2E-1, true, false, null, "x"], "b": {}, "c": [], "a": 0} "#;
        let want = Node::Object(vec![
            (
                "a".into(),
                Node::Array(vec![
                    n("1"),
                    n("-0.5e+3"),
                    n("2E-1"),
                    Node::Bool(true),
                    Node::Bool(false),
                    Node::Null,
                    s("x"),
                ]),
            ),
            ("b".into(), Node::Object(vec![])),
            ("c".into(), Node::Array(vec![])),
            ("a".into(), n("0")),
        ]);
        assert_eq!(parse_json(doc), Ok(want));
    }

    #[test]
    fn strings_decode_like_go() {
        let cases: &[(&[u8], &str)] = &[
            (br#""plain""#, "plain"),
            (br#""\"\\\/\b\f\n\r\t""#, "\"\\/\u{8}\u{c}\n\r\t"),
            (br#""\u00e9\u20AC""#, "é€"),
            (br#""\ud83d\ude00""#, "😀"),
            // Unpaired surrogates become U+FFFD; the next escape still counts.
            (br#""\ud800""#, "\u{FFFD}"),
            (br#""\udc00x""#, "\u{FFFD}x"),
            (br#""\ud800\u0041""#, "\u{FFFD}A"),
            (br#""\ud800\ud800\udc00""#, "\u{FFFD}\u{10000}"),
            ("\"é\"".as_bytes(), "é"),
            // Each byte that does not start a valid rune is replaced.
            (b"\"\xff\"", "\u{FFFD}"),
            (b"\"\xe2\x82\"", "\u{FFFD}\u{FFFD}"),
            (b"\"\xed\xa0\x80\"", "\u{FFFD}\u{FFFD}\u{FFFD}"),
            (b"\"\xc0\xaf\"", "\u{FFFD}\u{FFFD}"),
            (b"\"\xf0\x9f\x98\x80\"", "😀"),
            (b"\"\x7f\"", "\x7f"),
        ];
        for &(input, want) in cases {
            assert_eq!(parse_json(input), Ok(s(want)), "{input:?}");
        }
    }

    #[test]
    fn rejects_what_go_rejects() {
        for bad in [
            &b""[..],
            b"   ",
            b"{",
            b"{\"a\"}",
            b"{\"a\":}",
            b"{\"a\":1,}",
            b"[1,]",
            b"[1 2]",
            b"{a:1}",
            b"{'a':1}",
            b"01",
            b"-",
            b"1.",
            b".5",
            b"1e",
            b"1e+",
            b"+1",
            b"NaN",
            b"tru",
            b"nul",
            b"\"a",
            b"\"\x01\"",
            b"\"\\x\"",
            b"\"\\u12\"",
            b"\"\\u12g4\"",
            b"\"\\",
            b"{} {}",
            b"{} x",
            b"\xef\xbb\xbf{}",
            b"[1]]",
        ] {
            assert!(
                parse_json(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn errors_say_where() {
        let err = parse_json(b"{\n  \"a\": x}").unwrap_err();
        assert!(err.contains("line 2, column 8"), "{err}");
        assert!(err.contains("'x'"), "{err}");
        let err = parse_json(b"{\"a\":\x01}").unwrap_err();
        assert!(err.contains("byte 0x01"), "{err}");
        let err = parse_json(b"{} []").unwrap_err();
        assert!(err.contains("trailing data"), "{err}");
        assert!(parse_json(b"[1").unwrap_err().contains("end of input"));
    }

    #[test]
    fn deep_nesting_is_rejected_without_recursing_far() {
        let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert!(parse_json(deep.as_bytes()).unwrap_err().contains("nesting"));
        let ok = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(parse_json(ok.as_bytes()).is_ok());
    }

    #[test]
    fn toml_maps_onto_the_same_tree() {
        let doc =
            b"a = 1\nb = 1.0\nc = 'x'\nd = [true, false]\ne = 1e300\nf = 1979-05-27\n[t]\ng = -2\n";
        let node = parse_toml(doc).unwrap();
        let Node::Object(members) = node else {
            panic!("{node:?}")
        };
        assert_eq!(members[0], ("a".into(), n("1")));
        assert_eq!(members[1], ("b".into(), n("1.0")));
        assert_eq!(members[2], ("c".into(), s("x")));
        assert_eq!(
            members[3],
            (
                "d".into(),
                Node::Array(vec![Node::Bool(true), Node::Bool(false)])
            )
        );
        assert_eq!(members[4], ("e".into(), n("1e300")));
        assert!(matches!(members[5].1, Node::Object(_)), "{:?}", members[5]);
        assert_eq!(
            members[6],
            ("t".into(), Node::Object(vec![("g".into(), n("-2"))]))
        );
        assert!(parse_toml(b"a = ").is_err());
        assert!(parse_toml(b"a = 1\na = 2").is_err());
        assert!(parse_toml(b"a = '\xff'").unwrap_err().contains("UTF-8"));
    }

    #[test]
    fn kinds_name_each_variant() {
        let all = [
            Node::Null,
            Node::Bool(true),
            n("1"),
            s(""),
            Node::Array(vec![]),
            Node::Object(vec![]),
        ];
        let kinds: Vec<_> = all.iter().map(Node::kind).collect();
        assert_eq!(
            kinds,
            [
                "null",
                "a boolean",
                "a number",
                "a string",
                "an array",
                "an object"
            ]
        );
    }
}
