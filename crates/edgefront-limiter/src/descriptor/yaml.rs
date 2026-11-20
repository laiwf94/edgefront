//! The part of YAML that rate limit service configs use, read without a
//! YAML dependency: block mappings and sequences by indentation, plain,
//! single- and double-quoted scalars, flow sequences and mappings
//! (`[a, b]`, `{name: x}`), comments and `---` document markers. Anchors,
//! tags, multi-line scalars and several documents with content are
//! refused with the line, rather than read wrongly.

use std::fmt;

/// A parsed node, with the 1-based line it starts on.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    /// A scalar; `quoted` is false for plain ones, which may stand for a
    /// number, boolean or null.
    Scalar {
        text: String,
        quoted: bool,
        line: usize,
    },
    Seq {
        items: Vec<Node>,
        line: usize,
    },
    Map {
        entries: Vec<(String, Node)>,
        line: usize,
    },
    /// A key with nothing after it, or an empty document.
    Null {
        line: usize,
    },
}

impl Node {
    pub fn line(&self) -> usize {
        match self {
            Node::Scalar { line, .. }
            | Node::Seq { line, .. }
            | Node::Map { line, .. }
            | Node::Null { line } => *line,
        }
    }

    /// The scalar's text, if a scalar.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Node::Scalar { text, .. } => Some(text),
            _ => None,
        }
    }

    /// How Go's yaml.v2 would print the value in an error: the text of a
    /// scalar, otherwise a short description.
    pub fn describe(&self) -> String {
        match self {
            Node::Scalar { text, .. } => text.clone(),
            Node::Seq { .. } => "sequence".into(),
            Node::Map { .. } => "map".into(),
            Node::Null { .. } => "null".into(),
        }
    }
}

/// Why a document could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct YamlError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for YamlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "yaml: line {}: {}", self.line, self.message)
    }
}

fn err<T>(line: usize, message: impl Into<String>) -> Result<T, YamlError> {
    Err(YamlError {
        line,
        message: message.into(),
    })
}

struct Line<'a> {
    no: usize,
    indent: usize,
    text: &'a str,
}

/// The text without a comment: `#` at the start or after a space,
/// outside quotes.
fn strip_comment(s: &str) -> &str {
    let b = s.as_bytes();
    let (mut single, mut double) = (false, false);
    for i in 0..b.len() {
        match b[i] {
            b'\'' if !double => single = !single,
            b'"' if !single && (i == 0 || b[i - 1] != b'\\') => double = !double,
            b'#' if !single && !double && (i == 0 || b[i - 1] == b' ') => return &s[..i],
            _ => {}
        }
    }
    s
}

/// Parses one document. An empty one is [`Node::Null`].
pub fn parse(text: &str) -> Result<Node, YamlError> {
    let mut lines = Vec::new();
    let mut docs = 0;
    let mut content_docs = 0;
    for (i, raw) in text.lines().enumerate() {
        let no = i + 1;
        let body = strip_comment(raw).trim_end();
        if body.trim_start().starts_with('\t') || raw.starts_with('\t') {
            return err(no, "found character that cannot start any token");
        }
        if body == "---" || body == "..." {
            if !lines.is_empty() && docs == content_docs {
                docs += 1;
            }
            continue;
        }
        if body.trim().is_empty() {
            continue;
        }
        if docs > 0 && content_docs < docs && !lines.is_empty() {
            return err(no, "only one document is supported");
        }
        content_docs = docs;
        let indent = body.len() - body.trim_start().len();
        let t = body.trim_start();
        if t.starts_with('&')
            || t.starts_with('*')
            || t.starts_with('!')
            || t.contains(": &")
            || t.contains(": *")
            || t.contains(": !")
        {
            return err(no, "anchors, aliases and tags are not supported");
        }
        if t.starts_with('|') || t.starts_with('>') || t.ends_with(": |") || t.ends_with(": >") {
            return err(no, "block scalars are not supported");
        }
        lines.push(Line {
            no,
            indent,
            text: t,
        });
    }
    if lines.is_empty() {
        return Ok(Node::Null { line: 1 });
    }
    let mut p = Parser { lines, at: 0 };
    let first = p.lines[0].indent;
    let node = p.block(first)?;
    if p.at < p.lines.len() {
        let l = &p.lines[p.at];
        return err(l.no, "did not find expected key");
    }
    Ok(node)
}

struct Parser<'a> {
    lines: Vec<Line<'a>>,
    at: usize,
}

impl<'a> Parser<'a> {
    /// The block at indentation `indent` starting at the current line.
    fn block(&mut self, indent: usize) -> Result<Node, YamlError> {
        let l = &self.lines[self.at];
        if l.text == "-" || l.text.starts_with("- ") {
            self.seq(indent)
        } else if split_key(l.text).is_some() {
            self.map(indent)
        } else {
            let (no, text) = (l.no, l.text);
            self.at += 1;
            scalar_or_flow(text, no)
        }
    }

    fn seq(&mut self, indent: usize) -> Result<Node, YamlError> {
        let line = self.lines[self.at].no;
        let mut items = Vec::new();
        while self.at < self.lines.len() {
            let l = &self.lines[self.at];
            if l.indent < indent {
                break;
            }
            if l.indent > indent {
                return err(l.no, "bad indentation of a sequence entry");
            }
            let Some(rest) = l.text.strip_prefix('-') else {
                break;
            };
            let no = l.no;
            let rest = rest.trim_start();
            if rest.is_empty() {
                self.at += 1;
                match self.lines.get(self.at) {
                    Some(n) if n.indent > indent => {
                        let i = n.indent;
                        items.push(self.block(i)?);
                    }
                    _ => items.push(Node::Null { line: no }),
                }
                continue;
            }
            // `- key: value` opens a mapping at the column after "- ".
            let col = indent + (l.text.len() - rest.len());
            if split_key(rest).is_some() {
                self.lines[self.at].indent = col;
                self.lines[self.at].text = rest;
                items.push(self.map(col)?);
            } else {
                self.at += 1;
                items.push(scalar_or_flow(rest, no)?);
            }
        }
        Ok(Node::Seq { items, line })
    }

    fn map(&mut self, indent: usize) -> Result<Node, YamlError> {
        let line = self.lines[self.at].no;
        let mut entries: Vec<(String, Node)> = Vec::new();
        while self.at < self.lines.len() {
            let l = &self.lines[self.at];
            if l.indent < indent {
                break;
            }
            if l.indent > indent {
                return err(l.no, "mapping values are not allowed in this context");
            }
            let no = l.no;
            let Some((key, value)) = split_key(l.text) else {
                return err(no, "could not find expected ':'");
            };
            let key = match scalar_or_flow(key, no)? {
                Node::Scalar { text, .. } => text,
                _ => return err(no, "a key must be a scalar"),
            };
            if entries.iter().any(|(k, _)| *k == key) {
                return err(no, format!("duplicate key {key:?}"));
            }
            self.at += 1;
            let node = if value.is_empty() {
                match self.lines.get(self.at) {
                    Some(n) if n.indent > indent => {
                        let i = n.indent;
                        self.block(i)?
                    }
                    // A sequence may sit at the key's own indentation.
                    Some(n)
                        if n.indent == indent && (n.text == "-" || n.text.starts_with("- ")) =>
                    {
                        self.seq(indent)?
                    }
                    _ => Node::Null { line: no },
                }
            } else {
                scalar_or_flow(value, no)?
            };
            entries.push((key, node));
        }
        Ok(Node::Map { entries, line })
    }
}

/// `key: value` split at the first `: ` (or a trailing `:`) outside
/// quotes and brackets.
fn split_key(t: &str) -> Option<(&str, &str)> {
    let b = t.as_bytes();
    let (mut single, mut double, mut depth) = (false, false, 0i32);
    for i in 0..b.len() {
        match b[i] {
            b'\'' if !double => single = !single,
            b'"' if !single => double = !double,
            b'[' | b'{' if !single && !double => depth += 1,
            b']' | b'}' if !single && !double => depth -= 1,
            b':' if !single && !double && depth == 0 && (i + 1 == b.len() || b[i + 1] == b' ') => {
                if i == 0 {
                    return None;
                }
                return Some((t[..i].trim_end(), t[i + 1..].trim()));
            }
            _ => {}
        }
    }
    None
}

fn scalar_or_flow(t: &str, line: usize) -> Result<Node, YamlError> {
    let t = t.trim();
    if t.starts_with('[') || t.starts_with('{') {
        let mut f = Flow {
            s: t.as_bytes(),
            at: 0,
            line,
        };
        let n = f.value()?;
        f.ws();
        if f.at != f.s.len() {
            return err(line, "did not find expected end of flow collection");
        }
        return Ok(n);
    }
    scalar(t, line)
}

fn scalar(t: &str, line: usize) -> Result<Node, YamlError> {
    if let Some(inner) = t.strip_prefix('\'') {
        let Some(inner) = inner.strip_suffix('\'') else {
            return err(line, "found unexpected end of stream");
        };
        return Ok(Node::Scalar {
            text: inner.replace("''", "'"),
            quoted: true,
            line,
        });
    }
    if let Some(inner) = t.strip_prefix('"') {
        let Some(inner) = inner.strip_suffix('"') else {
            return err(line, "found unexpected end of stream");
        };
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some(o) => return err(line, format!("found unknown escape character {o:?}")),
                None => return err(line, "found unexpected end of stream"),
            }
        }
        return Ok(Node::Scalar {
            text: out,
            quoted: true,
            line,
        });
    }
    if t == "~" || t == "null" || t == "Null" || t == "NULL" {
        return Ok(Node::Null { line });
    }
    Ok(Node::Scalar {
        text: t.to_owned(),
        quoted: false,
        line,
    })
}

struct Flow<'a> {
    s: &'a [u8],
    at: usize,
    line: usize,
}

impl Flow<'_> {
    fn ws(&mut self) {
        while self.at < self.s.len() && self.s[self.at] == b' ' {
            self.at += 1;
        }
    }

    fn value(&mut self) -> Result<Node, YamlError> {
        self.ws();
        match self.s.get(self.at) {
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                loop {
                    self.ws();
                    if self.s.get(self.at) == Some(&b']') {
                        self.at += 1;
                        return Ok(Node::Seq {
                            items,
                            line: self.line,
                        });
                    }
                    items.push(self.value()?);
                    self.ws();
                    match self.s.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b']') => {}
                        _ => return err(self.line, "did not find expected ',' or ']'"),
                    }
                }
            }
            Some(b'{') => {
                self.at += 1;
                let mut entries: Vec<(String, Node)> = Vec::new();
                loop {
                    self.ws();
                    if self.s.get(self.at) == Some(&b'}') {
                        self.at += 1;
                        return Ok(Node::Map {
                            entries,
                            line: self.line,
                        });
                    }
                    let key = self.word(b":")?;
                    let key = match scalar(key.trim(), self.line)? {
                        Node::Scalar { text, .. } => text,
                        _ => return err(self.line, "a key must be a scalar"),
                    };
                    if self.s.get(self.at) != Some(&b':') {
                        return err(self.line, "did not find expected ':'");
                    }
                    self.at += 1;
                    let v = self.value()?;
                    if entries.iter().any(|(k, _)| *k == key) {
                        return err(self.line, format!("duplicate key {key:?}"));
                    }
                    entries.push((key, v));
                    self.ws();
                    match self.s.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b'}') => {}
                        _ => return err(self.line, "did not find expected ',' or '}'"),
                    }
                }
            }
            Some(_) => {
                let w = self.word(b",]}")?;
                scalar(w.trim(), self.line)
            }
            None => err(self.line, "found unexpected end of stream"),
        }
    }

    /// Text up to one of `stops` outside quotes.
    fn word(&mut self, stops: &[u8]) -> Result<String, YamlError> {
        let start = self.at;
        let (mut single, mut double) = (false, false);
        while self.at < self.s.len() {
            let c = self.s[self.at];
            match c {
                b'\'' if !double => single = !single,
                b'"' if !single => double = !double,
                _ if !single && !double && stops.contains(&c) => break,
                _ => {}
            }
            self.at += 1;
        }
        String::from_utf8(self.s[start..self.at].to_vec()).map_err(|_| YamlError {
            line: self.line,
            message: "invalid UTF-8".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> Node {
        Node::Scalar {
            text: text.into(),
            quoted: false,
            line: 0,
        }
    }

    /// The tree without line numbers, for comparisons.
    fn bare(n: Node) -> Node {
        match n {
            Node::Scalar { text, quoted, .. } => Node::Scalar {
                text,
                quoted,
                line: 0,
            },
            Node::Seq { items, .. } => Node::Seq {
                items: items.into_iter().map(bare).collect(),
                line: 0,
            },
            Node::Map { entries, .. } => Node::Map {
                entries: entries.into_iter().map(|(k, v)| (k, bare(v))).collect(),
                line: 0,
            },
            Node::Null { .. } => Node::Null { line: 0 },
        }
    }

    fn map(entries: Vec<(&str, Node)>) -> Node {
        Node::Map {
            entries: entries.into_iter().map(|(k, v)| (k.into(), v)).collect(),
            line: 0,
        }
    }

    fn seq(items: Vec<Node>) -> Node {
        Node::Seq { items, line: 0 }
    }

    #[test]
    fn block_maps_and_sequences_nest() {
        let doc = "---\n# a comment\ndomain: test # trailing\ndescriptors:\n  - key: a\n    value: 'x y'\n    descriptors:\n    - key: b\n      rate_limit: {unit: second, requests_per_unit: 5}\n  - key: c\n    replaces: [ {name: n1}, {name: \"n2\"} ]\n  -\n    key: d\nempty:\nlist:\n- 1\n- two\n---\n";
        let n = parse(doc).unwrap();
        assert_eq!(n.line(), 3);
        let want = map(vec![
            ("domain", s("test")),
            (
                "descriptors",
                seq(vec![
                    map(vec![
                        ("key", s("a")),
                        (
                            "value",
                            Node::Scalar {
                                text: "x y".into(),
                                quoted: true,
                                line: 0,
                            },
                        ),
                        (
                            "descriptors",
                            seq(vec![map(vec![
                                ("key", s("b")),
                                (
                                    "rate_limit",
                                    map(vec![("unit", s("second")), ("requests_per_unit", s("5"))]),
                                ),
                            ])]),
                        ),
                    ]),
                    map(vec![
                        ("key", s("c")),
                        (
                            "replaces",
                            seq(vec![
                                map(vec![("name", s("n1"))]),
                                map(vec![(
                                    "name",
                                    Node::Scalar {
                                        text: "n2".into(),
                                        quoted: true,
                                        line: 0,
                                    },
                                )]),
                            ]),
                        ),
                    ]),
                    map(vec![("key", s("d"))]),
                ]),
            ),
            ("empty", Node::Null { line: 0 }),
            ("list", seq(vec![s("1"), s("two")])),
        ]);
        assert_eq!(bare(n), want);
        assert_eq!(parse("").unwrap(), Node::Null { line: 1 });
        assert_eq!(parse("# only\n---\n").unwrap(), Node::Null { line: 1 });
        assert_eq!(
            bare(parse("a: 'it''s'\nb: \"q\\\"\\n\"\nc: x#y\n").unwrap()),
            map(vec![
                (
                    "a",
                    Node::Scalar {
                        text: "it's".into(),
                        quoted: true,
                        line: 0
                    }
                ),
                (
                    "b",
                    Node::Scalar {
                        text: "q\"\n".into(),
                        quoted: true,
                        line: 0
                    }
                ),
                ("c", s("x#y")),
            ])
        );
        assert_eq!(
            bare(parse("k: []\nm: {}\n").unwrap()),
            map(vec![("k", seq(vec![])), ("m", map(vec![]))])
        );
    }

    #[test]
    fn what_is_not_supported_is_refused_with_its_line() {
        for (doc, line, msg) in [
            (
                "a: 1\n\tb: 2\n",
                2,
                "found character that cannot start any token",
            ),
            (
                "a: &x 1\n",
                1,
                "anchors, aliases and tags are not supported",
            ),
            ("a: |\n  text\n", 1, "block scalars are not supported"),
            ("a: 1\n---\nb: 2\n", 3, "only one document is supported"),
            ("a: 1\na: 2\n", 2, "duplicate key \"a\""),
            ("a: 'open\n", 1, "found unexpected end of stream"),
            ("a: [1, 2\n", 1, "did not find expected ',' or ']'"),
            ("a: {b 1}\n", 1, "did not find expected ':'"),
            (
                "a:\n  b: 1\n c: 2\n",
                3,
                "mapping values are not allowed in this context",
            ),
            ("- a\n  - b\n", 2, "bad indentation of a sequence entry"),
            ("a: \"\\q\"\n", 1, "found unknown escape character 'q'"),
        ] {
            let e = parse(doc).unwrap_err();
            assert_eq!((e.line, e.message.as_str()), (line, msg), "{doc:?}");
        }
        assert_eq!(
            parse("a: 'x\n").unwrap_err().to_string(),
            "yaml: line 1: found unexpected end of stream"
        );
        assert_eq!(s("x").describe(), "x");
        assert_eq!(seq(vec![]).describe(), "sequence");
        assert_eq!(map(vec![]).describe(), "map");
        assert_eq!(Node::Null { line: 0 }.describe(), "null");
        assert_eq!(s("x").as_str(), Some("x"));
        assert_eq!(seq(vec![]).as_str(), None);
    }
}
