//! HTTP/2 on hyper's h2: the server side of the edge listener
//! ([`server`]).
//!
//! A TLS listener offers `h2` before `http/1.1` over ALPN when the
//! configuration has an `http2` block, and a cleartext one accepts HTTP/2
//! with prior knowledge (h2c) when the block sets `cleartext`. Either way
//! the connection is served by the same [`Handler`](crate::listener::Handler)
//! as HTTP/1.1, one request per stream.

pub mod server;

pub use server::{ALPN_H2, PREFACE, Settings};
