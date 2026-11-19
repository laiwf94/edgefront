//! One-off HTTP/1.1 requests for the harness's own control traffic: the
//! origin's counters and a target's readiness. Load never goes through
//! here; it goes through [`crate::load`].

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use http::{Request, header};
use http_body_util::{BodyExt, Empty};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

/// The most a control response may carry.
const MAX_BODY: usize = 1 << 20;

/// `GET path` on `addr` over a fresh connection, within `timeout`.
/// Returns the body of a 2xx response; anything else is an error naming
/// what went wrong.
pub async fn get(addr: SocketAddr, path: &str, timeout: Duration) -> Result<Bytes, String> {
    match tokio::time::timeout(timeout, exchange(addr, path)).await {
        Ok(result) => result,
        Err(_) => Err(format!("GET {path}: no answer within {timeout:?}")),
    }
}

async fn exchange(addr: SocketAddr, path: &str) -> Result<Bytes, String> {
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connect to {addr}: {e}"))?;
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| format!("handshake with {addr}: {e}"))?;
    tokio::spawn(connection);
    let request = Request::get(path)
        .header(header::HOST, addr.to_string())
        .body(Empty::<Bytes>::new())
        .map_err(|e| format!("GET {path}: {e}"))?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| format!("GET {path}: {e}"))?;
    let status = response.status();
    let mut body = response.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| format!("GET {path}: {e}"))?;
        if let Some(data) = frame.data_ref() {
            if bytes.len() + data.len() > MAX_BODY {
                return Err(format!("GET {path}: body over {MAX_BODY} bytes"));
            }
            bytes.extend_from_slice(data);
        }
    }
    if !status.is_success() {
        return Err(format!("GET {path}: status {}", status.as_u16()));
    }
    Ok(Bytes::from(bytes))
}

/// Polls `GET path` on `addr` every 100 ms until it answers 2xx, for up to
/// `within`. Returns the last error when it never does.
pub async fn wait_ready(addr: SocketAddr, path: &str, within: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let error = match get(addr, path, Duration::from_secs(2)).await {
            Ok(_) => return Ok(()),
            Err(e) => e,
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("not ready after {within:?}: {error}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::origin::Origin;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn waits_until_a_server_answers() {
        // Nothing listens yet: the wait fails with the connect error.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let err = wait_ready(addr, "/x", Duration::from_millis(250))
            .await
            .unwrap_err();
        assert!(
            err.contains("not ready") && err.contains("connect"),
            "{err}"
        );

        let origin = Origin::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        wait_ready(origin.addr(), "/__origin/healthz", Duration::from_secs(5))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn refuses_huge_bodies_and_silent_servers() {
        let origin = Origin::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let err = get(origin.addr(), "/obj/2000000/k1", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.contains("body over"), "{err}");

        // A server that accepts and never answers.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hold = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let err = get(addr, "/x", Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(err.contains("no answer"), "{err}");
        hold.abort();

        // A server that answers garbage.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = stream.write_all(b"nonsense\r\n\r\n").await;
        });
        let err = get(addr, "/x", Duration::from_secs(5)).await.unwrap_err();
        assert!(err.starts_with("GET /x"), "{err}");
    }
}
