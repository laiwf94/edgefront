//! A tonic echo service and client helpers, shared by the gRPC tests
//! through `#[path]`. Messages are raw bytes (a codec of its own instead
//! of prost and generated code), and the service is served by the edge's
//! own listener over cleartext HTTP/2.
//!
//! Methods of `echo.Echo`:
//! - `Unary`: the message back. `fail` answers `NOT_FOUND` with an
//!   `x-detail` trailer; `sleep:<ms>` waits first. The response carries
//!   the `grpc-timeout` the service received as `seen-timeout`.
//! - `ServerStream`: a message `n` answers `0`..`n-1`.
//! - `ClientStream`: every message joined with `,`.
//! - `Bidi`: each message upper-cased, as it arrives.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Buf, BufMut, Bytes};
use edgefront_core::config::Http2;
use edgefront_core::h2::Settings;
use edgefront_core::listener::{Handler, ServerLimits, serve};
use http_body_util::BodyExt;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::body::Incoming;
use tokio::net::TcpListener;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::codegen::tokio_stream::Stream;
use tonic::server::{
    ClientStreamingService, ServerStreamingService, StreamingService, UnaryService,
};
use tonic::{Request, Response, Status, Streaming};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type BoxFuture<T> = Pin<Box<dyn std::future::Future<Output = T> + Send>>;
pub type MessageStream = Pin<Box<dyn Stream<Item = Result<Bytes, Status>> + Send>>;

/// Messages as their bytes.
#[derive(Debug, Clone, Copy, Default)]
pub struct Raw;

pub struct RawEncoder;
pub struct RawDecoder;

impl Encoder for RawEncoder {
    type Item = Bytes;
    type Error = Status;

    fn encode(&mut self, item: Bytes, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        dst.put_slice(&item);
        Ok(())
    }
}

impl Decoder for RawDecoder {
    type Item = Bytes;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Bytes>, Status> {
        Ok(Some(src.copy_to_bytes(src.remaining())))
    }
}

impl Codec for Raw {
    type Encode = Bytes;
    type Decode = Bytes;
    type Encoder = RawEncoder;
    type Decoder = RawDecoder;

    fn encoder(&mut self) -> RawEncoder {
        RawEncoder
    }

    fn decoder(&mut self) -> RawDecoder {
        RawDecoder
    }
}

struct Unary;

impl UnaryService<Bytes> for Unary {
    type Response = Bytes;
    type Future = BoxFuture<Result<Response<Bytes>, Status>>;

    fn call(&mut self, req: Request<Bytes>) -> Self::Future {
        Box::pin(async move {
            let seen = req
                .metadata()
                .get("grpc-timeout")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none")
                .to_owned();
            let msg = req.into_inner();
            if &msg[..] == b"fail" {
                let mut status = Status::not_found("nothing here");
                status
                    .metadata_mut()
                    .insert("x-detail", "why".parse().unwrap());
                return Err(status);
            }
            if let Some(ms) = std::str::from_utf8(&msg)
                .ok()
                .and_then(|m| m.strip_prefix("sleep:"))
                .and_then(|n| n.parse().ok())
            {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            let mut resp = Response::new(msg);
            resp.metadata_mut()
                .insert("seen-timeout", seen.parse().unwrap());
            Ok(resp)
        })
    }
}

struct ServerStream;

impl ServerStreamingService<Bytes> for ServerStream {
    type Response = Bytes;
    type ResponseStream = MessageStream;
    type Future = BoxFuture<Result<Response<MessageStream>, Status>>;

    fn call(&mut self, req: Request<Bytes>) -> Self::Future {
        Box::pin(async move {
            let n: usize = std::str::from_utf8(req.get_ref())
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| Status::invalid_argument("not a count"))?;
            let items = (0..n).map(|i| Ok(Bytes::from(i.to_string())));
            let stream: MessageStream = Box::pin(tonic::codegen::tokio_stream::iter(items));
            Ok(Response::new(stream))
        })
    }
}

struct ClientStream;

impl ClientStreamingService<Bytes> for ClientStream {
    type Response = Bytes;
    type Future = BoxFuture<Result<Response<Bytes>, Status>>;

    fn call(&mut self, req: Request<Streaming<Bytes>>) -> Self::Future {
        Box::pin(async move {
            let mut stream = req.into_inner();
            let mut parts = Vec::new();
            while let Some(m) = stream.message().await? {
                parts.push(String::from_utf8_lossy(&m).into_owned());
            }
            Ok(Response::new(Bytes::from(parts.join(","))))
        })
    }
}

/// The messages a task sends, as a stream.
pub struct Received(pub tokio::sync::mpsc::Receiver<Result<Bytes, Status>>);

impl Stream for Received {
    type Item = Result<Bytes, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

struct Bidi;

impl StreamingService<Bytes> for Bidi {
    type Response = Bytes;
    type ResponseStream = MessageStream;
    type Future = BoxFuture<Result<Response<MessageStream>, Status>>;

    fn call(&mut self, req: Request<Streaming<Bytes>>) -> Self::Future {
        Box::pin(async move {
            let mut incoming = req.into_inner();
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            tokio::spawn(async move {
                while let Ok(Some(m)) = incoming.message().await {
                    let up = Bytes::from(m.to_ascii_uppercase());
                    if tx.send(Ok(up)).await.is_err() {
                        return;
                    }
                }
            });
            let stream: MessageStream = Box::pin(Received(rx));
            Ok(Response::new(stream))
        })
    }
}

/// The echo service as an edge listener handler.
pub struct Echo;

impl Handler for Echo {
    type Body = UnsyncBoxBody<Bytes, BoxError>;

    async fn call(
        self: Arc<Self>,
        req: http::Request<Incoming>,
        _: SocketAddr,
    ) -> http::Response<Self::Body> {
        let mut grpc = tonic::server::Grpc::new(Raw);
        let resp = match req.uri().path() {
            "/echo.Echo/Unary" => grpc.unary(Unary, req).await,
            "/echo.Echo/ServerStream" => grpc.server_streaming(ServerStream, req).await,
            "/echo.Echo/ClientStream" => grpc.client_streaming(ClientStream, req).await,
            "/echo.Echo/Bidi" => grpc.streaming(Bidi, req).await,
            _ => Status::unimplemented("no such method").into_http::<tonic::body::Body>(),
        };
        resp.map(|b| b.map_err(BoxError::from).boxed_unsync())
    }
}

/// Serves the echo service over cleartext HTTP/2 on a free port.
pub async fn start() -> SocketAddr {
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
        Arc::new(Echo),
        limits,
        std::future::pending(),
    ));
    addr
}

/// A gRPC client of `addr` over cleartext HTTP/2.
pub async fn client(addr: SocketAddr) -> tonic::client::Grpc<tonic::transport::Channel> {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .expect("a gRPC channel");
    tonic::client::Grpc::new(channel)
}

pub fn path(method: &str) -> http::uri::PathAndQuery {
    format!("/echo.Echo/{method}").parse().unwrap()
}

/// A unary call of `message`, with `timeout` as its `grpc-timeout` when
/// set.
pub async fn unary(
    grpc: &mut tonic::client::Grpc<tonic::transport::Channel>,
    message: &str,
    timeout: Option<Duration>,
) -> Result<Response<Bytes>, Status> {
    grpc.ready()
        .await
        .map_err(|e| Status::unavailable(e.to_string()))?;
    let mut req = Request::new(Bytes::from(message.to_owned()));
    // The deadline goes as the header alone: tonic's own timer would race
    // the proxy's, and the tests are about the proxy's.
    if let Some(t) = timeout {
        let value = format!("{}m", t.as_millis());
        req.metadata_mut()
            .insert("grpc-timeout", value.parse().unwrap());
    }
    grpc.unary(req, path("Unary"), Raw).await
}

/// Sends `messages` as a client stream and returns the answer.
pub async fn client_stream(
    grpc: &mut tonic::client::Grpc<tonic::transport::Channel>,
    messages: &[&str],
) -> Result<Bytes, Status> {
    grpc.ready()
        .await
        .map_err(|e| Status::unavailable(e.to_string()))?;
    let items: Vec<Bytes> = messages
        .iter()
        .map(|m| Bytes::from(m.to_string()))
        .collect();
    let stream = tonic::codegen::tokio_stream::iter(items);
    let resp = grpc
        .client_streaming(Request::new(stream), path("ClientStream"), Raw)
        .await?;
    Ok(resp.into_inner())
}

/// Asks for `n` messages and collects them.
pub async fn server_stream(
    grpc: &mut tonic::client::Grpc<tonic::transport::Channel>,
    n: usize,
) -> Result<Vec<String>, Status> {
    grpc.ready()
        .await
        .map_err(|e| Status::unavailable(e.to_string()))?;
    let resp = grpc
        .server_streaming(
            Request::new(Bytes::from(n.to_string())),
            path("ServerStream"),
            Raw,
        )
        .await?;
    let mut stream = resp.into_inner();
    let mut out = Vec::new();
    while let Some(m) = stream.message().await? {
        out.push(String::from_utf8_lossy(&m).into_owned());
    }
    Ok(out)
}

/// Sends `messages` one at a time over a bidirectional call, reading each
/// answer before sending the next.
pub async fn bidi(
    grpc: &mut tonic::client::Grpc<tonic::transport::Channel>,
    messages: &[&str],
) -> Result<Vec<String>, Status> {
    grpc.ready()
        .await
        .map_err(|e| Status::unavailable(e.to_string()))?;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Status>>(1);
    struct Outgoing(tokio::sync::mpsc::Receiver<Result<Bytes, Status>>);
    impl Stream for Outgoing {
        type Item = Bytes;
        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Bytes>> {
            self.0.poll_recv(cx).map(|m| m.and_then(Result::ok))
        }
    }
    tx.send(Ok(Bytes::from(messages[0].to_owned())))
        .await
        .unwrap();
    let resp = grpc
        .streaming(Request::new(Outgoing(rx)), path("Bidi"), Raw)
        .await?;
    let mut incoming = resp.into_inner();
    let mut out = Vec::new();
    for (i, _) in messages.iter().enumerate() {
        let m = incoming
            .message()
            .await?
            .ok_or_else(|| Status::internal("stream ended early"))?;
        out.push(String::from_utf8_lossy(&m).into_owned());
        if let Some(next) = messages.get(i + 1) {
            tx.send(Ok(Bytes::from(next.to_string()))).await.unwrap();
        }
    }
    drop(tx);
    if incoming.message().await?.is_some() {
        return Err(Status::internal("more answers than messages"));
    }
    Ok(out)
}
