//! The rate limit service over gRPC: a tonic client calling
//! `ShouldRateLimit` on edgefront's own HTTP/2 listener, as Envoy's global
//! rate limit filter does, with the messages encoded by the limiter's
//! prost codec on both ends.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use edgefront_core::config::Http2;
use edgefront_core::h2::Settings;
use edgefront_core::listener::{Handler, ServerLimits, serve};
use edgefront_limiter::descriptor::{DescriptorConfig, DescriptorLimiter};
use edgefront_limiter::service::proto::envoy::extensions::common::ratelimit::v3::{
    RateLimitDescriptor, rate_limit_descriptor,
};
use edgefront_limiter::service::proto::envoy::service::ratelimit::v3::rate_limit_response::Code;
use edgefront_limiter::service::proto::envoy::service::ratelimit::v3::{
    RateLimitRequest, RateLimitResponse,
};
use edgefront_limiter::service::rls::{
    ProstCodec, RateLimitService, ResponseHeaders, SHOULD_RATE_LIMIT,
};
use hyper::body::Incoming;
use tokio::net::TcpListener;
use tonic::transport::Channel;

const CONFIG: &str = "
domain: edge
descriptors:
  - key: remote_address
    rate_limit: {unit: hour, requests_per_unit: 50}
  - key: path
    value: /login
    rate_limit: {unit: hour, requests_per_unit: 2, name: login}
";

struct Rls(RateLimitService);

impl Handler for Rls {
    type Body = tonic::body::Body;

    async fn call(
        self: Arc<Self>,
        req: http::Request<Incoming>,
        _: SocketAddr,
    ) -> http::Response<tonic::body::Body> {
        self.0.call(req.map(tonic::body::Body::new)).await
    }
}

async fn start() -> SocketAddr {
    let cfg = DescriptorConfig::load(&[("edge.yaml", CONFIG)], false).unwrap();
    let svc = RateLimitService::new(Arc::new(DescriptorLimiter::new(cfg, 1 << 16)))
        .with_response_headers(ResponseHeaders::default());
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let limits = ServerLimits {
        http2: Some(Settings::from(&Http2 {
            cleartext: true,
            ..Http2::default()
        })),
        ..ServerLimits::default()
    };
    tokio::spawn(serve(
        socket,
        Arc::new(Rls(svc)),
        limits,
        std::future::pending(),
    ));
    addr
}

async fn client(addr: SocketAddr) -> tonic::client::Grpc<Channel> {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .expect("a gRPC channel");
    tonic::client::Grpc::new(channel)
}

async fn unary<Req, Resp>(
    grpc: &mut tonic::client::Grpc<Channel>,
    path: &str,
    msg: Req,
) -> Result<Resp, tonic::Status>
where
    Req: prost::Message + Send + Sync + 'static,
    Resp: prost::Message + Default + Send + Sync + 'static,
{
    grpc.ready().await.unwrap();
    grpc.unary(
        tonic::Request::new(msg),
        path.parse().unwrap(),
        ProstCodec::<Req, Resp>::default(),
    )
    .await
    .map(tonic::Response::into_inner)
}

async fn should_rate_limit(
    grpc: &mut tonic::client::Grpc<Channel>,
    req: RateLimitRequest,
) -> Result<RateLimitResponse, tonic::Status> {
    unary(grpc, SHOULD_RATE_LIMIT, req).await
}

fn request(entries: &[&[(&str, &str)]]) -> RateLimitRequest {
    RateLimitRequest {
        domain: "edge".into(),
        descriptors: entries
            .iter()
            .map(|e| RateLimitDescriptor {
                entries: e
                    .iter()
                    .map(|(k, v)| rate_limit_descriptor::Entry {
                        key: (*k).into(),
                        value: (*v).into(),
                    })
                    .collect(),
                ..RateLimitDescriptor::default()
            })
            .collect(),
        hits_addend: 0,
    }
}

#[tokio::test]
async fn envoy_style_calls_get_ok_then_over_limit() {
    let addr = start().await;
    let mut grpc = client(addr).await;
    let req = || request(&[&[("remote_address", "10.0.0.1")], &[("path", "/login")]]);
    for remaining in [1, 0] {
        let r = should_rate_limit(&mut grpc, req()).await.unwrap();
        assert_eq!(r.overall_code, i32::from(Code::Ok));
        assert_eq!(r.statuses.len(), 2);
        assert_eq!(r.statuses[1].limit_remaining, remaining);
        let limit = r.statuses[1].current_limit.as_ref().unwrap();
        assert_eq!((limit.name.as_str(), limit.requests_per_unit), ("login", 2));
        let reset = r.statuses[1].duration_until_reset.unwrap();
        assert!((1..=3600).contains(&reset.seconds), "{reset:?}");
        let headers: Vec<_> = r
            .response_headers_to_add
            .iter()
            .map(|h| (h.key.as_str(), h.value.as_str()))
            .collect();
        assert_eq!(headers[0], ("RateLimit-Limit", "2"));
        assert_eq!(headers[1], ("RateLimit-Remaining", &*remaining.to_string()));
        assert_eq!(headers[2].0, "RateLimit-Reset");
    }
    let r = should_rate_limit(&mut grpc, req()).await.unwrap();
    assert_eq!(r.overall_code, i32::from(Code::OverLimit));
    assert_eq!(r.statuses[0].code, i32::from(Code::Ok));
    assert_eq!(r.statuses[0].limit_remaining, 47);
    assert_eq!(r.statuses[1].code, i32::from(Code::OverLimit));
}

#[tokio::test]
async fn concurrent_streams_share_one_counter() {
    let addr = start().await;
    let grpc = client(addr).await;
    // 64 streams on one connection, 4 calls each, against a limit of 50.
    let tasks: Vec<_> = (0..64)
        .map(|_| {
            let mut grpc = grpc.clone();
            tokio::spawn(async move {
                let mut ok = 0;
                for _ in 0..4 {
                    let r = should_rate_limit(&mut grpc, request(&[&[("remote_address", "a")]]))
                        .await
                        .unwrap();
                    ok += usize::from(r.overall_code == i32::from(Code::Ok));
                }
                ok
            })
        })
        .collect();
    let mut ok = 0;
    for t in tasks {
        ok += t.await.unwrap();
    }
    assert_eq!(ok, 50);
}

#[tokio::test]
async fn bad_calls_get_grpc_errors() {
    let addr = start().await;
    let mut grpc = client(addr).await;
    let mut req = request(&[&[("path", "/login")]]);
    req.domain.clear();
    let e = should_rate_limit(&mut grpc, req).await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    assert_eq!(e.message(), "rate limit domain must not be empty");
    let e = should_rate_limit(&mut grpc, request(&[]))
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    // Another method of the service, or another service.
    for path in [
        "/envoy.service.ratelimit.v3.RateLimitService/Other",
        "/echo.Echo/Unary",
    ] {
        let e = unary::<RateLimitRequest, RateLimitResponse>(&mut grpc, path, request(&[]))
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unimplemented);
        assert!(e.message().contains(path), "{}", e.message());
    }
    // Bytes that are not a RateLimitRequest: field 1 (domain) as a varint.
    let e = unary::<RawBytes, RateLimitResponse>(
        &mut grpc,
        SHOULD_RATE_LIMIT,
        RawBytes(Bytes::from_static(&[0x08, 0x01])),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code(), tonic::Code::Internal);
    // The connection still serves calls.
    let r = should_rate_limit(&mut grpc, request(&[&[("path", "/x")]]))
        .await
        .unwrap();
    assert_eq!(r.overall_code, i32::from(Code::Ok));
}

/// A message that encodes as the bytes it holds.
#[derive(Clone, Debug, Default, PartialEq)]
struct RawBytes(Bytes);

impl prost::Message for RawBytes {
    fn encode_raw(&self, buf: &mut impl bytes::BufMut) {
        buf.put_slice(&self.0);
    }

    fn merge_field(
        &mut self,
        _: u32,
        _: prost::encoding::WireType,
        _: &mut impl bytes::Buf,
        _: prost::encoding::DecodeContext,
    ) -> Result<(), prost::DecodeError> {
        unreachable!("only encoded")
    }

    fn encoded_len(&self) -> usize {
        self.0.len()
    }

    fn clear(&mut self) {
        self.0 = Bytes::new();
    }
}
