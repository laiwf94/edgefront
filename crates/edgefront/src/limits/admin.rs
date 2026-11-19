//! The admin API: HTTP/1.1 on the `admin_socket` Unix socket, in place of
//! pacerd's one-line protocol on the same socket (`src/pacerd/admin.c`).
//! The socket keeps pacerd's rules: a stale file left by a crashed run is
//! replaced, one that still answers belongs to a running daemon and is
//! refused, the file is mode 0600, at most 8 clients are served at once
//! and each has 2 seconds to send its request.
//!
//! | request | answer |
//! |---|---|
//! | `GET /ping` | `ok pong` |
//! | `GET /stats` | the counters, in pacerd's `stats` text ([`super::stats`]) |
//! | `POST /reload` | `ok reloaded`, or `error <why>` with 409 |
//! | `POST /drain` | `ok draining` |
//!
//! Anything else is `404 error unknown command`, and a method other than
//! the one listed is `405`.

use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio::net::UnixListener;
use tokio::sync::Semaphore;

use super::daemon::Daemon;
use super::stats;

/// At most this many admin clients are served at once: pacerd's 8.
pub const CLIENTS: usize = 8;

/// How long a client has to send its request head: pacerd's 2 seconds.
pub const DEADLINE: Duration = Duration::from_secs(2);

/// Why the admin socket could not be opened, as pacerd logs it.
#[derive(Debug)]
pub struct OpenError {
    pub msg: &'static str,
    pub error: String,
}

/// The bound admin socket.
pub struct Admin {
    listener: UnixListener,
}

/// Binds the admin socket at `path`, clearing a stale file first.
pub fn open(path: &Path) -> Result<Admin, OpenError> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            return Err(OpenError {
                msg: "admin_socket path exists and is not a socket",
                error: "File exists".into(),
            });
        }
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(OpenError {
                msg: "admin_socket is in use by a running daemon",
                error: "Address already in use".into(),
            });
        }
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path).map_err(|e| OpenError {
        msg: "cannot listen on admin socket",
        error: e.to_string(),
    })?;
    // pacerd set its umask before bind so the file never existed with
    // wider rights; without libc the mode is set right after, before the
    // first accept.
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(path);
        return Err(OpenError {
            msg: "cannot listen on admin socket",
            error: e.to_string(),
        });
    }
    Ok(Admin { listener })
}

fn text(status: StatusCode, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(body.into()));
    *r.status_mut() = status;
    r.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r
}

/// Answers one admin request.
pub fn answer(d: &Daemon, method: &Method, path: &str) -> Response<Full<Bytes>> {
    let want = |m: Method| -> Option<Response<Full<Bytes>>> {
        (*method != m).then(|| text(StatusCode::METHOD_NOT_ALLOWED, "error method not allowed\n"))
    };
    match path {
        "/ping" => want(Method::GET).unwrap_or_else(|| text(StatusCode::OK, "ok pong\n")),
        "/stats" => want(Method::GET).unwrap_or_else(|| text(StatusCode::OK, stats::render(d))),
        "/reload" => want(Method::POST).unwrap_or_else(|| match d.reload("admin") {
            Ok(()) => text(StatusCode::OK, "ok reloaded\n"),
            Err(e) => text(StatusCode::CONFLICT, format!("error {e}\n")),
        }),
        "/drain" => want(Method::POST).unwrap_or_else(|| {
            d.drain("admin");
            text(StatusCode::OK, "ok draining\n")
        }),
        _ => text(StatusCode::NOT_FOUND, "error unknown command\n"),
    }
}

/// Serves the admin API until the task is aborted.
pub async fn serve(admin: Admin, d: Arc<Daemon>) {
    let slots = Arc::new(Semaphore::new(CLIENTS));
    loop {
        let Ok((stream, _)) = admin.listener.accept().await else {
            continue;
        };
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            // pacerd's answer to a ninth client.
            let mut s = stream;
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = s
                    .write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 11\r\nconnection: close\r\n\r\nerror busy\n")
                    .await;
            });
            continue;
        };
        let d = Arc::clone(&d);
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                let r = answer(&d, req.method(), req.uri().path());
                async move { Ok::<_, Infallible>(r) }
            });
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(DEADLINE)
                .keep_alive(false)
                .serve_connection(TokioIo::new(stream), svc);
            let _ = tokio::time::timeout(DEADLINE * 5, conn).await;
            drop(slot);
        });
    }
}
