//! The HTTP/2 server connection: the settings it advertises, read from the
//! configuration's `http2` block, and the detection of a cleartext client
//! that starts with the HTTP/2 preface.
//!
//! Every limit is advertised in the server's first `SETTINGS` frame and
//! enforced by h2: a client that opens more than `max_concurrent_streams`
//! streams gets `REFUSED_STREAM` on the extra ones, a frame larger than
//! `max_frame_size` is a `FRAME_SIZE_ERROR`, a header block that decodes
//! to more than `max_header_list_size` bytes is answered `431` on its
//! stream, and a peer that overruns a window is a `FLOW_CONTROL_ERROR`.
//! The connection window is raised past the protocol's 65,535 bytes with a
//! `WINDOW_UPDATE` right after the preface.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes};
use hyper::server::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

use crate::config::Http2;

/// The ALPN protocol identifier for HTTP/2 over TLS (RFC 9113 section 3.2).
pub const ALPN_H2: &[u8] = b"h2";

/// The connection preface a client sends first (RFC 9113 section 3.4).
pub const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// The settings a listener's HTTP/2 connections advertise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub max_concurrent_streams: u32,
    pub initial_stream_window: u32,
    pub initial_connection_window: u32,
    pub max_frame_size: u32,
    pub max_header_list_size: u32,
    /// Accept prior-knowledge HTTP/2 on a cleartext listener.
    pub cleartext: bool,
    /// Grow the receive windows to the measured bandwidth-delay product
    /// instead of keeping the configured ones ([`super::flow`]).
    pub adaptive_window: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings::from(&Http2::default())
    }
}

impl From<&Http2> for Settings {
    fn from(h: &Http2) -> Self {
        Settings {
            max_concurrent_streams: h.max_concurrent_streams,
            initial_stream_window: h.initial_stream_window,
            initial_connection_window: h.initial_connection_window,
            max_frame_size: h.max_frame_size,
            max_header_list_size: h.max_header_list_size,
            cleartext: h.cleartext,
            adaptive_window: h.adaptive_window,
        }
    }
}

impl Settings {
    /// How the windows are sized.
    pub fn flow(&self) -> super::Flow {
        if self.adaptive_window {
            super::Flow::Adaptive
        } else {
            super::Flow::Fixed {
                stream: self.initial_stream_window,
                connection: self.initial_connection_window,
            }
        }
    }

    /// The hyper builder for a server connection with these settings,
    /// with windows as [`flow`](Self::flow) says.
    pub fn builder(&self) -> http2::Builder<TokioExecutor> {
        let (adaptive, stream, connection) = self.flow().hyper();
        let mut b = http2::Builder::new(TokioExecutor::new());
        b.timer(TokioTimer::new())
            .initial_stream_window_size(stream)
            .initial_connection_window_size(connection)
            .adaptive_window(adaptive)
            .max_concurrent_streams(self.max_concurrent_streams)
            .max_frame_size(self.max_frame_size)
            .max_header_list_size(self.max_header_list_size)
            // Enough to keep a full stream window in flight per stream
            // without holding unbounded response data per connection.
            .max_send_buf_size(self.initial_stream_window.min(1 << 20) as usize);
        b
    }
}

/// Reads from `stream` until it is clear whether the client opened with
/// the HTTP/2 preface. Returns whether it did and every byte read, which
/// the connection must see again ([`Rewind`]). A client that closes or
/// sends a different first byte stops the read early; the caller bounds
/// the wait with the header read timeout.
pub async fn sniff_preface<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<(bool, Bytes)> {
    let mut seen = Vec::with_capacity(PREFACE.len());
    while seen.len() < PREFACE.len() {
        let mut chunk = [0u8; 24];
        let want = PREFACE.len() - seen.len();
        let n = stream.read(&mut chunk[..want]).await?;
        if n == 0 {
            break;
        }
        seen.extend_from_slice(&chunk[..n]);
        if !PREFACE.starts_with(&seen) {
            return Ok((false, Bytes::from(seen)));
        }
    }
    let h2 = seen.len() == PREFACE.len();
    Ok((h2, Bytes::from(seen)))
}

/// Answers a connection that did not open with the preface: an empty
/// `SETTINGS` (the server's preface must come first), then `GOAWAY` with
/// last stream 0 and `PROTOCOL_ERROR`, then a clean close.
pub async fn refuse_preface<S: AsyncWrite + Unpin>(stream: &mut S) -> io::Result<()> {
    const SETTINGS: [u8; 9] = [0, 0, 0, 0x4, 0, 0, 0, 0, 0];
    const GOAWAY: [u8; 17] = [0, 0, 8, 0x7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x1];
    stream.write_all(&SETTINGS).await?;
    stream.write_all(&GOAWAY).await?;
    stream.shutdown().await
}

/// A stream that yields `prefix` before reading on: the bytes
/// [`sniff_preface`] took.
pub struct Rewind<S> {
    prefix: Bytes,
    inner: S,
}

impl<S> Rewind<S> {
    pub fn new(prefix: Bytes, inner: S) -> Self {
        Rewind { prefix, inner }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Rewind<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.prefix.has_remaining() {
            let n = this.prefix.len().min(buf.remaining());
            buf.put_slice(&this.prefix[..n]);
            this.prefix.advance(n);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Rewind<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn settings_follow_the_config() {
        let s = Settings::default();
        assert_eq!(s.max_concurrent_streams, 100);
        assert_eq!(s.initial_stream_window, 1 << 20);
        assert_eq!(s.initial_connection_window, 4 << 20);
        assert_eq!(s.max_frame_size, 16_384);
        assert_eq!(s.max_header_list_size, 65_536);
        assert!(!s.cleartext);
        assert!(!s.adaptive_window);
        let _ = s.builder();
        let adaptive = Settings {
            adaptive_window: true,
            ..s
        };
        assert_eq!(adaptive.flow(), super::super::Flow::Adaptive);
        let _ = adaptive.builder();
    }

    async fn sniff(input: &[u8]) -> (bool, Vec<u8>, Vec<u8>) {
        let (mut client, mut server) = tokio::io::duplex(64);
        tokio::io::AsyncWriteExt::write_all(&mut client, input)
            .await
            .unwrap();
        drop(client);
        let (h2, seen) = sniff_preface(&mut server).await.unwrap();
        let mut rest = Vec::new();
        Rewind::new(seen.clone(), server)
            .read_to_end(&mut rest)
            .await
            .unwrap();
        (h2, seen.to_vec(), rest)
    }

    #[tokio::test]
    async fn the_preface_is_detected_and_replayed() {
        let mut input = PREFACE.to_vec();
        input.extend_from_slice(b"\0\0\0\x04\0\0\0\0\0");
        let (h2, seen, all) = sniff(&input).await;
        assert!(h2);
        assert_eq!(seen, PREFACE);
        assert_eq!(all, input);
    }

    #[tokio::test]
    async fn http1_stops_at_the_first_difference() {
        // What arrived in one read is kept whole and replayed.
        let (h2, seen, all) = sniff(b"POST / HTTP/1.1\r\n\r\n").await;
        assert!(!h2);
        assert_eq!(seen, b"POST / HTTP/1.1\r\n\r\n");
        assert_eq!(all, b"POST / HTTP/1.1\r\n\r\n");
        // A byte at a time, the read stops at the first byte that differs.
        let (mut client, mut server) = tokio::io::duplex(1);
        let writer = tokio::spawn(async move {
            for b in b"PRX more" {
                let _ = tokio::io::AsyncWriteExt::write_all(&mut client, &[*b]).await;
            }
        });
        let (h2, seen) = sniff_preface(&mut server).await.unwrap();
        assert!(!h2);
        assert_eq!(&seen[..], b"PRX");
        drop(server);
        let _ = writer.await;
    }

    #[tokio::test]
    async fn a_bad_preface_gets_goaway_with_protocol_error() {
        let (mut client, mut server) = tokio::io::duplex(64);
        refuse_preface(&mut server).await.unwrap();
        drop(server);
        let mut out = Vec::new();
        client.read_to_end(&mut out).await.unwrap();
        assert_eq!(out.len(), 26);
        assert_eq!(out[3], 0x4, "SETTINGS first");
        assert_eq!(&out[9..13], &[0, 0, 8, 0x7], "then an 8-byte GOAWAY");
        assert_eq!(&out[22..26], &[0, 0, 0, 1], "PROTOCOL_ERROR");
    }

    #[tokio::test]
    async fn a_short_preface_is_not_http2() {
        let (h2, seen, all) = sniff(&PREFACE[..10]).await;
        assert!(!h2);
        assert_eq!(seen, &PREFACE[..10]);
        assert_eq!(all, &PREFACE[..10]);
        let (h2, seen, _) = sniff(b"").await;
        assert!(!h2);
        assert!(seen.is_empty());
    }
}
