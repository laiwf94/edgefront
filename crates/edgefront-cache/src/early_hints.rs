//! `103 Early Hints` (RFC 8297) from stored HTML.
//!
//! A stored HTML page keeps the `Link` fields its origin sent, and the
//! ones that tell a browser to start fetching (`preload`,
//! `modulepreload`, `preconnect`) are the page's hints. When the cache has
//! to go upstream for such a page, to revalidate a stale copy or to fetch
//! it again, it sends those hints as a `103` first, so the browser loads
//! the page's styles and scripts during the origin's think time instead
//! of after it. The final response carries the same `Link` fields, as RFC
//! 8297 asks of hints that a client may not have acted on.
//!
//! A hint goes out only on HTTP/1.1 connections the server can send
//! interim responses on ([`edgefront_core::interim`]); a client on HTTP/1.0
//! or HTTP/2 gets the final response alone.

use http::StatusCode;
use http::header::{self, HeaderMap, HeaderValue};

use crate::object::CachedObject;

/// The `rel` values worth an early hint.
const HINT_RELS: [&str; 3] = ["preload", "modulepreload", "preconnect"];
/// At most this many `Link` values go in one 103.
pub const MAX_HINTS: usize = 32;

/// `103 Early Hints`.
pub const EARLY_HINTS: StatusCode = match StatusCode::from_u16(103) {
    Ok(s) => s,
    Err(_) => panic!("103 is a status code"),
};

/// Whether one `Link` value (`<uri>; param; ...`) is a hint: a target in
/// angle brackets and a `rel` naming one of [`HINT_RELS`].
fn is_hint(link: &str) -> bool {
    let link = link.trim();
    let Some(rest) = link.strip_prefix('<') else {
        return false;
    };
    let Some(end) = rest.find('>') else {
        return false;
    };
    rest[end + 1..].split(';').skip(1).any(|param| {
        let Some((k, v)) = param.split_once('=') else {
            return false;
        };
        k.trim().eq_ignore_ascii_case("rel")
            && v.trim()
                .trim_matches('"')
                .split_ascii_whitespace()
                .any(|r| HINT_RELS.iter().any(|h| h.eq_ignore_ascii_case(r)))
    })
}

/// Splits a `Link` field line into its values, at commas outside angle
/// brackets and quotes.
fn link_values(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut angle, mut quoted) = (0, false, false);
    for (i, c) in line.char_indices() {
        match c {
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            '"' if !angle => quoted = !quoted,
            ',' if !angle && !quoted => {
                out.push(&line[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&line[start..]);
    out
}

/// Whether a stored response is HTML.
fn is_html(obj: &CachedObject) -> bool {
    obj.headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("text/html"))
}

/// The early hints of a stored HTML page: its preload, modulepreload and
/// preconnect `Link` values, at most [`MAX_HINTS`]. `None` for anything
/// else.
pub fn hints(obj: &CachedObject) -> Option<HeaderMap> {
    if obj.status != StatusCode::OK || !is_html(obj) {
        return None;
    }
    let mut out = HeaderMap::new();
    for line in obj.headers.get_all(header::LINK) {
        let Ok(text) = line.to_str() else { continue };
        for value in link_values(text).into_iter().filter(|v| is_hint(v)) {
            if out.len() == MAX_HINTS {
                break;
            }
            if let Ok(v) = HeaderValue::from_str(value.trim()) {
                out.append(header::LINK, v);
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::tests::object;

    #[test]
    fn only_fetch_hints_of_html_are_kept() {
        let page = object(
            &[
                ("content-type", "text/html; charset=utf-8"),
                (
                    "link",
                    "</app.css>; rel=preload; as=style, </x,y.js>; rel=\"modulepreload\"",
                ),
                ("link", "<https://cdn.example>; rel=preconnect"),
                ("link", "</next>; rel=next, </a.css>; rel=stylesheet, junk"),
                ("link", "</q.js>; title=\"a, rel=preload\""),
            ],
            &[],
            "<html>",
        );
        let h = hints(&page).unwrap();
        let got: Vec<_> = h
            .get_all(header::LINK)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(
            got,
            [
                "</app.css>; rel=preload; as=style",
                "</x,y.js>; rel=\"modulepreload\"",
                "<https://cdn.example>; rel=preconnect",
            ]
        );
        let css = object(
            &[("content-type", "text/css"), ("link", "</a>; rel=preload")],
            &[],
            "a{}",
        );
        assert!(hints(&css).is_none());
        let bare = object(&[("content-type", "text/html")], &[], "<html>");
        assert!(hints(&bare).is_none());
        let many: Vec<String> = (0..50).map(|i| format!("</{i}.js>; rel=preload")).collect();
        let lots = object(
            &[("content-type", "text/html"), ("link", &many.join(", "))],
            &[],
            "<html>",
        );
        assert_eq!(hints(&lots).unwrap().len(), MAX_HINTS);
    }
}
