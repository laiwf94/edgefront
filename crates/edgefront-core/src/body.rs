//! The boxed response body that edgefront handlers return, and builders
//! for the small bodies a handler generates itself.

use bytes::Bytes;
use http::header::{self, HeaderValue};
use http::{Response, StatusCode};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};

pub use crate::http1::BoxError;

/// A response body of any kind: buffered, streamed from an origin or empty.
pub type Body = BoxBody<Bytes, BoxError>;

/// A body that holds `bytes`.
pub fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}

/// A body with no bytes.
pub fn empty() -> Body {
    Empty::new().map_err(|never| match never {}).boxed()
}

/// A plain-text response that must not be cached, for errors and status
/// pages a node generates itself.
pub fn text(status: StatusCode, message: &'static str) -> Response<Body> {
    let mut resp = Response::new(full(message));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::body::Body as _;

    async fn collect(body: Body) -> Bytes {
        body.collect().await.unwrap().to_bytes()
    }

    #[tokio::test]
    async fn builders() {
        assert_eq!(collect(full("abc")).await, "abc");
        let e = empty();
        assert!(e.is_end_stream());
        assert_eq!(collect(e).await, "");
        let resp = text(StatusCode::BAD_GATEWAY, "no origin\n");
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            resp.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(collect(resp.into_body()).await, "no origin\n");
    }
}
