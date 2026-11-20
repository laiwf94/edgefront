//! What a request costs, when it is not one: its body bytes, a GraphQL
//! query's complexity, or a number the upstream reports afterwards.
//!
//! - [`Cost::Fixed`]: every request costs the same.
//! - [`Cost::RequestBytes`]: the body's `Content-Length`. A request
//!   without one (chunked, or none) costs `missing`, so a byte limit is
//!   not dodged by leaving the length out.
//! - [`Cost::Graphql`]: [`graphql_complexity`] of the query: each field
//!   costs 1, and a field with a `first`, `last` or `limit` argument
//!   multiplies what is under it by that number, as GitHub's GraphQL API
//!   scores its queries. A query that does not parse, or is deeper or
//!   larger than [`GRAPHQL_MAX_DEPTH`] and [`GRAPHQL_MAX_TOKENS`], costs
//!   [`GRAPHQL_UNPARSED`]: more than a reasonable query, so a malformed
//!   one cannot be free.
//! - [`Cost::ResponseHeader`]: charged after the response, from the
//!   number the upstream puts in a header (a billing unit, rows scanned,
//!   tokens generated). Before the upstream answers, a request is only
//!   refused if its key is already in debt; once the answer is in, the
//!   header's cost is booked with [`crate::Table::book`], so the debt
//!   holds back the key's next requests until it is paid off. A missing
//!   or unreadable header costs `missing`.
//!
//! The decision is still the exact GCRA on the cost, so a byte limit of
//! 1 MiB a second admits 1 MiB a second, within the rounding of its
//! interval to 1/64 ns.

use std::fmt;

/// The deepest GraphQL selection [`graphql_complexity`] reads.
pub const GRAPHQL_MAX_DEPTH: usize = 32;
/// The most tokens it reads.
pub const GRAPHQL_MAX_TOKENS: usize = 100_000;
/// The cost of a query it cannot score.
pub const GRAPHQL_UNPARSED: u64 = 1000;
/// The largest multiplier one argument may give.
pub const GRAPHQL_MAX_MULTIPLIER: u64 = 10_000;

/// Where a request's cost comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cost {
    Fixed(u64),
    RequestBytes {
        missing: u64,
    },
    /// The query from the `query` parameter of a GET. A POST body is not
    /// visible to the request phase of the proxy's filters, so a POST
    /// costs `missing` there; [`graphql_complexity`] scores a body for a
    /// caller that has it.
    Graphql {
        missing: u64,
    },
    /// Lowercase header name.
    ResponseHeader {
        name: String,
        missing: u64,
    },
}

impl Cost {
    /// Reads `1`, `"request_bytes"`, `"graphql"` or
    /// `"response_header:x-cost"`, with `missing` the cost when the
    /// source has nothing to say.
    pub fn parse(text: &str, missing: u64) -> Result<Cost, String> {
        if let Ok(n) = text.parse::<u64>() {
            return if n == 0 {
                Err("a fixed cost must be at least 1".into())
            } else {
                Ok(Cost::Fixed(n))
            };
        }
        match text.split_once(':') {
            None if text == "request_bytes" => Ok(Cost::RequestBytes { missing }),
            None if text == "graphql" => Ok(Cost::Graphql { missing }),
            Some(("response_header", name))
                if !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
            {
                Ok(Cost::ResponseHeader {
                    name: name.to_ascii_lowercase(),
                    missing,
                })
            }
            _ => Err(format!(
                "{text:?} is not a cost; write a number, \"request_bytes\", \"graphql\" or \"response_header:NAME\""
            )),
        }
    }

    /// Whether the cost is known only after the response.
    pub fn after_response(&self) -> bool {
        matches!(self, Cost::ResponseHeader { .. })
    }

    /// The text [`Cost::parse`] reads.
    pub fn text(&self) -> String {
        match self {
            Cost::Fixed(n) => n.to_string(),
            Cost::RequestBytes { .. } => "request_bytes".into(),
            Cost::Graphql { .. } => "graphql".into(),
            Cost::ResponseHeader { name, .. } => format!("response_header:{name}"),
        }
    }
}

/// A header value as a cost: decimal digits, at most 2^50.
pub fn header_cost(value: Option<&[u8]>, missing: u64) -> u64 {
    value
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n <= 1 << 50)
        .unwrap_or(missing)
}

/// Why a query could not be scored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphqlError {
    Syntax(String),
    TooDeep,
    TooLarge,
}

impl fmt::Display for GraphqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphqlError::Syntax(m) => write!(f, "{m}"),
            GraphqlError::TooDeep => write!(f, "nested deeper than {GRAPHQL_MAX_DEPTH}"),
            GraphqlError::TooLarge => write!(f, "longer than {GRAPHQL_MAX_TOKENS} tokens"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok<'a> {
    Name(&'a str),
    Int(u64),
    Punct(u8),
    Other,
}

fn tokens(q: &str) -> Result<Vec<Tok<'_>>, GraphqlError> {
    let b = q.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if out.len() > GRAPHQL_MAX_TOKENS {
            return Err(GraphqlError::TooLarge);
        }
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' | b',' | 0xEF | 0xBB | 0xBF => i += 1,
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' => {
                let block = b[i..].starts_with(b"\"\"\"");
                i += if block { 3 } else { 1 };
                loop {
                    if i >= b.len() {
                        return Err(GraphqlError::Syntax("unterminated string".into()));
                    }
                    if block && b[i..].starts_with(b"\"\"\"") {
                        i += 3;
                        break;
                    }
                    if !block && b[i] == b'"' {
                        i += 1;
                        break;
                    }
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                out.push(Tok::Other);
            }
            b'.' if b[i..].starts_with(b"...") => {
                out.push(Tok::Punct(b'.'));
                i += 3;
            }
            b'{' | b'}' | b'(' | b')' | b':' | b'[' | b']' | b'!' | b'$' | b'@' | b'=' | b'|'
            | b'&' => {
                out.push(Tok::Punct(c));
                i += 1;
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                i += 1;
                while i < b.len()
                    && (b[i].is_ascii_alphanumeric()
                        || b[i] == b'.'
                        || b[i] == b'+'
                        || b[i] == b'-')
                {
                    i += 1;
                }
                match q[start..i].parse::<u64>() {
                    Ok(n) => out.push(Tok::Int(n)),
                    Err(_) => out.push(Tok::Other),
                }
            }
            _ if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                out.push(Tok::Name(&q[start..i]));
            }
            _ => {
                return Err(GraphqlError::Syntax(format!(
                    "unexpected character at byte {i}"
                )));
            }
        }
    }
    Ok(out)
}

struct Doc<'a> {
    t: Vec<Tok<'a>>,
    at: usize,
}

/// A selection set as read: fields with their multiplier and children,
/// and fragment spreads by name.
#[derive(Debug, Default)]
struct Set<'a> {
    fields: Vec<(u64, Set<'a>)>,
    spreads: Vec<&'a str>,
}

impl<'a> Doc<'a> {
    fn peek(&self) -> Option<&Tok<'a>> {
        self.t.get(self.at)
    }

    fn punct(&mut self, p: u8) -> bool {
        if self.peek() == Some(&Tok::Punct(p)) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, p: u8) -> Result<(), GraphqlError> {
        if self.punct(p) {
            Ok(())
        } else {
            Err(GraphqlError::Syntax(format!("expected {:?}", p as char)))
        }
    }

    fn name(&mut self) -> Result<&'a str, GraphqlError> {
        match self.peek() {
            Some(Tok::Name(n)) => {
                let n = *n;
                self.at += 1;
                Ok(n)
            }
            _ => Err(GraphqlError::Syntax("expected a name".into())),
        }
    }

    /// Skips a balanced `(...)` or `[...]`, reading `first`, `last` and
    /// `limit` arguments as integers on the way.
    fn arguments(&mut self) -> Result<u64, GraphqlError> {
        let mut mult = 1u64;
        if !self.punct(b'(') {
            return Ok(1);
        }
        let mut depth = 1;
        while depth > 0 {
            match self.peek().cloned() {
                None => return Err(GraphqlError::Syntax("unclosed arguments".into())),
                Some(Tok::Punct(b'(' | b'[' | b'{')) => depth += 1,
                Some(Tok::Punct(b')' | b']' | b'}')) => depth -= 1,
                Some(Tok::Name(n)) if depth == 1 && matches!(n, "first" | "last" | "limit") => {
                    if self.t.get(self.at + 1) == Some(&Tok::Punct(b':'))
                        && let Some(Tok::Int(v)) = self.t.get(self.at + 2)
                    {
                        mult = (*v).clamp(1, GRAPHQL_MAX_MULTIPLIER);
                        self.at += 2;
                    }
                }
                _ => {}
            }
            self.at += 1;
        }
        Ok(mult)
    }

    fn directives(&mut self) -> Result<(), GraphqlError> {
        while self.punct(b'@') {
            self.name()?;
            self.arguments()?;
        }
        Ok(())
    }

    fn set(&mut self, depth: usize) -> Result<Set<'a>, GraphqlError> {
        if depth > GRAPHQL_MAX_DEPTH {
            return Err(GraphqlError::TooDeep);
        }
        self.expect(b'{')?;
        let mut set = Set::default();
        while !self.punct(b'}') {
            if self.punct(b'.') {
                // `... on Type { }`, `... @dir { }` or `...Name`.
                if self.peek() == Some(&Tok::Name("on")) {
                    self.at += 1;
                    self.name()?;
                }
                if let Some(Tok::Name(n)) = self.peek() {
                    let n = *n;
                    self.at += 1;
                    self.directives()?;
                    set.spreads.push(n);
                    continue;
                }
                self.directives()?;
                let inner = self.set(depth + 1)?;
                set.fields.extend(inner.fields);
                set.spreads.extend(inner.spreads);
                continue;
            }
            // A field, or an alias and its field.
            self.name()?;
            if self.punct(b':') {
                self.name()?;
            }
            let mult = self.arguments()?;
            self.directives()?;
            let children = if self.peek() == Some(&Tok::Punct(b'{')) {
                self.set(depth + 1)?
            } else {
                Set::default()
            };
            set.fields.push((mult, children));
        }
        Ok(set)
    }
}

fn score(set: &Set<'_>, frags: &[(&str, Set<'_>)], depth: usize) -> Result<u64, GraphqlError> {
    if depth > GRAPHQL_MAX_DEPTH {
        return Err(GraphqlError::TooDeep);
    }
    let mut total = 0u64;
    for (mult, children) in &set.fields {
        let inner = score(children, frags, depth + 1)?;
        total = total.saturating_add(1u64.saturating_add(mult.saturating_mul(inner)));
    }
    for name in &set.spreads {
        let f = frags
            .iter()
            .find(|(n, _)| n == name)
            .ok_or_else(|| GraphqlError::Syntax(format!("unknown fragment {name}")))?;
        total = total.saturating_add(score(&f.1, frags, depth + 1)?);
    }
    Ok(total)
}

/// The complexity of every operation in a GraphQL document, summed: each
/// field 1, plus its children times its `first`, `last` or `limit`
/// argument (1 without one, capped at [`GRAPHQL_MAX_MULTIPLIER`]).
/// Fragments are expanded where they are spread; a fragment spread
/// inside itself is refused as too deep.
pub fn graphql_complexity(query: &str) -> Result<u64, GraphqlError> {
    let mut d = Doc {
        t: tokens(query)?,
        at: 0,
    };
    let mut ops = Vec::new();
    let mut frags = Vec::new();
    while d.peek().is_some() {
        match d.peek() {
            Some(Tok::Punct(b'{')) => ops.push(d.set(0)?),
            Some(Tok::Name("fragment")) => {
                d.at += 1;
                let name = d.name()?;
                if d.name()? != "on" {
                    return Err(GraphqlError::Syntax("expected on".into()));
                }
                d.name()?;
                d.directives()?;
                frags.push((name, d.set(0)?));
            }
            Some(Tok::Name("query" | "mutation" | "subscription")) => {
                d.at += 1;
                if let Some(Tok::Name(_)) = d.peek() {
                    d.at += 1;
                }
                // Variable definitions are skipped like arguments.
                d.arguments()?;
                d.directives()?;
                ops.push(d.set(0)?);
            }
            _ => {
                return Err(GraphqlError::Syntax(
                    "expected an operation or fragment".into(),
                ));
            }
        }
    }
    if ops.is_empty() {
        return Err(GraphqlError::Syntax("no operation".into()));
    }
    let mut total = 0u64;
    for op in &ops {
        total = total.saturating_add(score(op, &frags, 0)?);
    }
    Ok(total)
}

/// [`graphql_complexity`], or [`GRAPHQL_UNPARSED`] for a query it
/// cannot score.
pub fn graphql_cost(query: &str) -> u64 {
    graphql_complexity(query).unwrap_or(GRAPHQL_UNPARSED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcra::{Gcra, Rate};

    #[test]
    fn costs_parse() {
        assert_eq!(Cost::parse("3", 0).unwrap(), Cost::Fixed(3));
        assert_eq!(
            Cost::parse("request_bytes", 7).unwrap(),
            Cost::RequestBytes { missing: 7 }
        );
        assert_eq!(
            Cost::parse("graphql", 5).unwrap(),
            Cost::Graphql { missing: 5 }
        );
        let c = Cost::parse("response_header:X-Cost", 1).unwrap();
        assert_eq!(
            c,
            Cost::ResponseHeader {
                name: "x-cost".into(),
                missing: 1
            }
        );
        assert!(c.after_response() && !Cost::Fixed(1).after_response());
        for t in ["3", "request_bytes", "graphql", "response_header:x-cost"] {
            assert_eq!(Cost::parse(t, 1).unwrap().text(), t);
        }
        assert_eq!(
            Cost::parse("0", 1).unwrap_err(),
            "a fixed cost must be at least 1"
        );
        assert!(Cost::parse("response_header:", 1).is_err());
        assert!(Cost::parse("response_header:a b", 1).is_err());
        assert!(
            Cost::parse("bytes", 1)
                .unwrap_err()
                .contains("is not a cost")
        );
        assert_eq!(header_cost(Some(b" 42 "), 1), 42);
        assert_eq!(header_cost(Some(b"-1"), 1), 1);
        assert_eq!(header_cost(Some(b"x"), 9), 9);
        assert_eq!(header_cost(None, 9), 9);
        assert_eq!(header_cost(Some(b"99999999999999999"), 2), 2);
    }

    #[test]
    fn graphql_queries_are_scored_like_githubs() {
        assert_eq!(graphql_complexity("{ viewer { login } }").unwrap(), 2);
        // 1 (repository) + 1 (issues) + 50 x (1 node + 1 title + 1 author + 1 login)...
        let q = "query Q($n: Int = 5) { repository(owner: \"a\", name: \"b\") { issues(first: 50) { nodes { title author { login } } } } }";
        assert_eq!(graphql_complexity(q).unwrap(), 1 + 1 + 50 * (1 + 1 + 1 + 1));
        // Nested pages multiply.
        let q = "{ a(first: 10) { b(last: 10) { c } } }";
        assert_eq!(graphql_complexity(q).unwrap(), 1 + 10 * (1 + 10));
        // Aliases, fragments, inline fragments, directives, comments.
        let q = "# list\nquery { x: users(limit: 3) { ...F ... on Admin { level } name @include(if: true) } }\nfragment F on User { id email }";
        assert_eq!(graphql_complexity(q).unwrap(), 1 + 3 * (2 + 1 + 1));
        assert_eq!(
            graphql_complexity("mutation { add(input: {first: 9}) { id } }").unwrap(),
            2
        );
        assert_eq!(graphql_complexity("{ a } { b }").unwrap(), 2);
        assert_eq!(
            graphql_complexity("{ a(first: 999999999) { b } }").unwrap(),
            1 + GRAPHQL_MAX_MULTIPLIER
        );
        assert_eq!(
            graphql_complexity("{ s(q: \"\"\"x { y\"\"\" ) }").unwrap(),
            1
        );
    }

    #[test]
    fn bad_queries_cost_the_unparsed_price() {
        for q in [
            "",
            "{ a ",
            "{ a(first: 1 }",
            "x",
            "{ ...Missing }",
            "{ \"open }",
            "fragment F on T { ...F } { ...F }",
            "{ ? }",
            "fragment F at T { a }",
        ] {
            assert!(graphql_complexity(q).is_err(), "{q:?}");
            assert_eq!(graphql_cost(q), GRAPHQL_UNPARSED);
        }
        let deep = format!("{}{}", "{ a ".repeat(40), "}".repeat(40));
        assert_eq!(
            graphql_complexity(&deep).unwrap_err(),
            GraphqlError::TooDeep
        );
        let long = format!("{{ {} }}", "a ".repeat(GRAPHQL_MAX_TOKENS + 10));
        assert_eq!(
            graphql_complexity(&long).unwrap_err(),
            GraphqlError::TooLarge
        );
        assert_eq!(GraphqlError::TooDeep.to_string(), "nested deeper than 32");
        assert_eq!(
            GraphqlError::TooLarge.to_string(),
            "longer than 100000 tokens"
        );
        assert_eq!(graphql_cost("{ a }"), 1);
    }

    /// A byte limit charged by request size admits its rate within 1%:
    /// 10 s of random-sized bodies, offered at twice the rate, against
    /// 1 MiB a second with a burst of 64 KiB.
    #[test]
    fn a_byte_limit_admits_its_rate_within_one_percent() {
        let rate = Rate::new(1 << 20, 1_000_000_000, 64 << 10);
        let g = Gcra::new(&rate).unwrap();
        let mut x = 7u64;
        let (mut now, mut admitted) = (1_000_000_000u64, 0u64);
        let end = now + 10_000_000_000;
        while now < end {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let size = 512 + x % 8192;
            if g.take(now, size).unwrap().allowed {
                admitted += size;
            }
            // About 2 MiB a second offered.
            now += size * 1_000_000_000 / (2 << 20);
        }
        let want = 10.0 * f64::from(1 << 20) + f64::from(64 << 10);
        let err = (admitted as f64 - want).abs() / want;
        assert!(
            err < 0.01,
            "admitted {admitted} of {want}: {:.3}%",
            err * 100.0
        );
    }
}
