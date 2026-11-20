//! gRPC through the proxy: telling a gRPC call apart, its deadline, and
//! the proxy's own errors in gRPC's terms.
//!
//! A request is gRPC when its `content-type` is `application/grpc`, alone
//! or with a `+codec` suffix such as `+proto` or `+json` (the gRPC over
//! HTTP/2 protocol). Such a call needs HTTP/2 to its endpoint, for the
//! trailers that carry `grpc-status`: a cluster with `protocol` `http2` or
//! `auto`. Messages and trailers stream through as frames; nothing is
//! buffered.
//!
//! - **Deadline.** A `grpc-timeout` header (1 to 8 digits and a unit:
//!   `H`, `M`, `S`, `m`, `u` or `n`) bounds the call. The proxy holds the
//!   call to the sooner of that and the cluster's `timeout`, and the
//!   endpoint is sent the time left, in milliseconds, so it can give up
//!   when the client will. A malformed value is ignored.
//! - **Proxy errors.** When the proxy answers a gRPC call itself (no
//!   route, nothing healthy, a timeout, a refused or broken endpoint), the
//!   answer is a gRPC "Trailers-Only" response: HTTP 200 with
//!   `content-type: application/grpc`, a `grpc-status` and a
//!   percent-encoded `grpc-message`, and no body. The status follows the
//!   gRPC project's HTTP-to-gRPC mapping: 404 is `UNIMPLEMENTED`, 502 and
//!   503 `UNAVAILABLE`, 504 `DEADLINE_EXCEEDED`, 403 `PERMISSION_DENIED`,
//!   400 `INTERNAL`, and anything else `UNKNOWN`. The proxy's headers
//!   (`X-Request-Id`, `X-Steer-Overflow`) stay.

use std::time::Duration;

use edgefront_core::body::{self, Body};
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Response, StatusCode};

/// The deadline header (gRPC over HTTP/2, "Timeout").
pub const GRPC_TIMEOUT: HeaderName = HeaderName::from_static("grpc-timeout");
pub const GRPC_STATUS: HeaderName = HeaderName::from_static("grpc-status");
pub const GRPC_MESSAGE: HeaderName = HeaderName::from_static("grpc-message");

/// gRPC status codes the proxy answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Code {
    Unknown = 2,
    DeadlineExceeded = 4,
    PermissionDenied = 7,
    Unimplemented = 12,
    Internal = 13,
    Unavailable = 14,
}

/// Whether a request with `headers` is a gRPC call.
pub fn is_grpc(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            let ct = ct.trim_start();
            ct.len() >= 16
                && ct[..16].eq_ignore_ascii_case("application/grpc")
                && matches!(ct.as_bytes().get(16), None | Some(b'+' | b';'))
        })
}

/// A `grpc-timeout` value as a duration, or `None` when it is malformed.
pub fn parse_timeout(value: &[u8]) -> Option<Duration> {
    let (digits, unit) = value.split_at(value.len().checked_sub(1)?);
    if digits.is_empty() || digits.len() > 8 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n: u64 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    Some(match unit[0] {
        b'H' => Duration::from_secs(n * 3600),
        b'M' => Duration::from_secs(n * 60),
        b'S' => Duration::from_secs(n),
        b'm' => Duration::from_millis(n),
        b'u' => Duration::from_micros(n),
        b'n' => Duration::from_nanos(n),
        _ => return None,
    })
}

/// The call's own deadline, from its `grpc-timeout`.
pub fn timeout(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(&GRPC_TIMEOUT)
        .and_then(|v| parse_timeout(v.as_bytes()))
}

/// Rewrites `grpc-timeout` to `left`, in whole milliseconds rounded up so
/// a last fraction is not dropped to zero.
pub fn set_timeout(headers: &mut HeaderMap, left: Duration) {
    let ms = left.as_nanos().div_ceil(1_000_000).min(99_999_999);
    headers.insert(
        GRPC_TIMEOUT,
        HeaderValue::from_str(&format!("{ms}m")).expect("digits and a unit"),
    );
}

/// The gRPC status for an HTTP status the proxy answered with.
pub fn code_for(status: StatusCode) -> Code {
    match status.as_u16() {
        400 => Code::Internal,
        403 => Code::PermissionDenied,
        404 => Code::Unimplemented,
        502 | 503 => Code::Unavailable,
        504 => Code::DeadlineExceeded,
        _ => Code::Unknown,
    }
}

/// Percent-encodes a `grpc-message`: printable ASCII except `%` stays,
/// everything else becomes `%XX` of its UTF-8 bytes.
pub fn encode_message(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    for &b in message.as_bytes() {
        if (0x20..=0x7e).contains(&b) && b != b'%' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The proxy's own answer `resp` (with `message`) turned into a gRPC
/// Trailers-Only response, keeping its other headers.
pub fn trailers_only(resp: Response<Body>, message: &str) -> Response<Body> {
    let (mut head, _) = resp.into_parts();
    let code = code_for(head.status);
    head.status = StatusCode::OK;
    let h = &mut head.headers;
    h.remove(header::CONTENT_LENGTH);
    h.remove(header::CACHE_CONTROL);
    h.remove(header::X_CONTENT_TYPE_OPTIONS);
    h.remove(header::CONNECTION);
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc"),
    );
    h.insert(GRPC_STATUS, HeaderValue::from(code as u16));
    h.insert(
        GRPC_MESSAGE,
        HeaderValue::from_str(&encode_message(message)).expect("percent-encoded ASCII"),
    );
    Response::from_parts(head, body::empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_content_types() {
        let h = |ct: &str| {
            let mut m = HeaderMap::new();
            m.insert(header::CONTENT_TYPE, HeaderValue::from_str(ct).unwrap());
            m
        };
        for yes in [
            "application/grpc",
            "application/grpc+proto",
            "application/grpc+json",
            "Application/GRPC",
            "application/grpc; charset=utf-8",
        ] {
            assert!(is_grpc(&h(yes)), "{yes}");
        }
        for no in [
            "application/grpc-web",
            "application/grpcx",
            "application/json",
            "text/grpc",
            "",
        ] {
            assert!(!is_grpc(&h(no)), "{no}");
        }
        assert!(!is_grpc(&HeaderMap::new()));
    }

    #[test]
    fn timeouts_as_the_spec_writes_them() {
        let t = |s: &str| parse_timeout(s.as_bytes());
        assert_eq!(t("1H"), Some(Duration::from_secs(3600)));
        assert_eq!(t("2M"), Some(Duration::from_secs(120)));
        assert_eq!(t("30S"), Some(Duration::from_secs(30)));
        assert_eq!(t("100m"), Some(Duration::from_millis(100)));
        assert_eq!(t("250u"), Some(Duration::from_micros(250)));
        assert_eq!(t("99999999n"), Some(Duration::from_nanos(99_999_999)));
        assert_eq!(t("0m"), Some(Duration::ZERO));
        for bad in ["", "m", "100", "123456789m", "10x", "-1S", "1.5S", "1 S"] {
            assert_eq!(t(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_time_left_is_sent_in_milliseconds() {
        let mut h = HeaderMap::new();
        set_timeout(&mut h, Duration::from_micros(1500));
        assert_eq!(h.get(&GRPC_TIMEOUT).unwrap(), "2m");
        set_timeout(&mut h, Duration::ZERO);
        assert_eq!(h.get(&GRPC_TIMEOUT).unwrap(), "0m");
        set_timeout(&mut h, Duration::from_secs(1_000_000));
        assert_eq!(h.get(&GRPC_TIMEOUT).unwrap(), "99999999m");
        assert_eq!(timeout(&h), Some(Duration::from_millis(99_999_999)));
    }

    #[test]
    fn statuses_follow_the_http_mapping() {
        assert_eq!(code_for(StatusCode::NOT_FOUND), Code::Unimplemented);
        assert_eq!(code_for(StatusCode::BAD_GATEWAY), Code::Unavailable);
        assert_eq!(code_for(StatusCode::SERVICE_UNAVAILABLE), Code::Unavailable);
        assert_eq!(
            code_for(StatusCode::GATEWAY_TIMEOUT),
            Code::DeadlineExceeded
        );
        assert_eq!(code_for(StatusCode::FORBIDDEN), Code::PermissionDenied);
        assert_eq!(code_for(StatusCode::BAD_REQUEST), Code::Internal);
        assert_eq!(code_for(StatusCode::METHOD_NOT_ALLOWED), Code::Unknown);
    }

    #[test]
    fn messages_are_percent_encoded() {
        assert_eq!(encode_message("no route"), "no route");
        assert_eq!(encode_message("100% sure"), "100%25 sure");
        assert_eq!(encode_message("caf\u{e9}\n"), "caf%C3%A9%0A");
    }

    #[test]
    fn a_proxy_error_becomes_trailers_only() {
        let mut resp = Response::new(body::full("{}"));
        *resp.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        resp.headers_mut()
            .insert("x-request-id", HeaderValue::from_static("abc"));
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        let out = trailers_only(resp, "no healthy upstream is available");
        assert_eq!(out.status(), StatusCode::OK);
        let h = out.headers();
        assert_eq!(h["content-type"], "application/grpc");
        assert_eq!(h["grpc-status"], "14");
        assert_eq!(h["grpc-message"], "no healthy upstream is available");
        assert_eq!(h["x-request-id"], "abc");
        assert!(h.get(header::CACHE_CONTROL).is_none());
        assert!(hyper::body::Body::is_end_stream(out.body()));
    }
}
