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
//! | `GET /clients?top=N&listener=NAME` | the N clients with the most connections open (10 by default), per listener |
//! | `GET /client?addr=IP&listener=NAME` | one address's limits as tracked now, per listener |
//! | `POST /reload` | `ok reloaded`, or `error <why>` with 409 |
//! | `POST /drain` | `ok draining` |
//! | `POST /ban?addr=IP&for=SECONDS&reason=WORD` | bans the address until then (`reason` defaults to `manual`): `ok banned` |
//! | `POST /unban?addr=IP` | `ok unbanned`, or 404 when it was not banned |
//! | `GET /bans` | one ban a line: `<address> <seconds left> <reason>` |
//!
//! Bans are checked at accept, before any limit, against the connecting
//! peer's address, and kept across reloads; with `EDGEFRONT_BANS_FILE`
//! set they are loaded from that file at start and saved to it on every
//! change, so they survive a restart too.
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
pub fn answer(
    d: &Daemon,
    method: &Method,
    path: &str,
    query: Option<&str>,
) -> Response<Full<Bytes>> {
    let want = |m: Method| -> Option<Response<Full<Bytes>>> {
        (*method != m).then(|| text(StatusCode::METHOD_NOT_ALLOWED, "error method not allowed\n"))
    };
    match path {
        "/ping" => want(Method::GET).unwrap_or_else(|| text(StatusCode::OK, "ok pong\n")),
        "/stats" => want(Method::GET).unwrap_or_else(|| text(StatusCode::OK, stats::render(d))),
        "/clients" => want(Method::GET).unwrap_or_else(|| {
            let top = match param(query, "top").map(|t| t.parse::<usize>()) {
                None => 10,
                Some(Ok(n)) => n,
                Some(Err(_)) => {
                    return text(StatusCode::BAD_REQUEST, "error top must be a number\n");
                }
            };
            reply(stats::top(d, param(query, "listener").as_deref(), top))
        }),
        "/client" => want(Method::GET).unwrap_or_else(|| match param(query, "addr") {
            None => text(StatusCode::BAD_REQUEST, "error addr is required\n"),
            Some(addr) => reply(stats::client(d, param(query, "listener").as_deref(), &addr)),
        }),
        "/reload" => want(Method::POST).unwrap_or_else(|| match d.reload("admin") {
            Ok(()) => text(StatusCode::OK, "ok reloaded\n"),
            Err(e) => text(StatusCode::CONFLICT, format!("error {e}\n")),
        }),
        "/drain" => want(Method::POST).unwrap_or_else(|| {
            d.drain("admin");
            text(StatusCode::OK, "ok draining\n")
        }),
        "/ban" => want(Method::POST).unwrap_or_else(|| {
            let addr = param(query, "addr").and_then(|a| a.parse::<std::net::IpAddr>().ok());
            let secs = param(query, "for")
                .and_then(|f| f.parse::<u64>().ok())
                .filter(|&s| s > 0);
            let (Some(addr), Some(secs)) = (addr, secs) else {
                return text(
                    StatusCode::BAD_REQUEST,
                    "error addr (an IP address) and for (seconds) are required\n",
                );
            };
            let reason = param(query, "reason").unwrap_or_else(|| "manual".into());
            let until = d
                .clock
                .now_ns()
                .saturating_add(secs.saturating_mul(1_000_000_000));
            match d.bans.ban(&super::daemon::ban_key(addr), until, &reason) {
                Ok(()) => {
                    save_bans(d);
                    text(StatusCode::OK, "ok banned\n")
                }
                Err(e) => text(StatusCode::BAD_REQUEST, format!("error {e}\n")),
            }
        }),
        "/unban" => want(Method::POST).unwrap_or_else(|| {
            match param(query, "addr").and_then(|a| a.parse::<std::net::IpAddr>().ok()) {
                None => text(StatusCode::BAD_REQUEST, "error addr is required\n"),
                Some(addr) if d.bans.unban(&super::daemon::ban_key(addr)) => {
                    save_bans(d);
                    text(StatusCode::OK, "ok unbanned\n")
                }
                Some(_) => text(StatusCode::NOT_FOUND, "error not banned\n"),
            }
        }),
        "/bans" => want(Method::GET).unwrap_or_else(|| {
            let now = d.clock.now_ns();
            d.bans.sweep(now);
            let mut out = String::new();
            for (k, b) in d.bans.list() {
                if b.until_ns <= now {
                    continue;
                }
                let addr = match k.len() {
                    4 => std::net::IpAddr::from(
                        <[u8; 4]>::try_from(k.as_slice()).unwrap_or_default(),
                    ),
                    16 => std::net::IpAddr::from(
                        <[u8; 16]>::try_from(k.as_slice()).unwrap_or_default(),
                    ),
                    _ => continue,
                };
                out.push_str(&format!(
                    "{addr} {} {}\n",
                    (b.until_ns - now).div_ceil(1_000_000_000),
                    b.reason
                ));
            }
            text(StatusCode::OK, out)
        }),
        _ => text(StatusCode::NOT_FOUND, "error unknown command\n"),
    }
}

/// Saves the bans to `EDGEFRONT_BANS_FILE`, if set, logging a failure.
fn save_bans(d: &Daemon) {
    if let Some(f) = &d.bans_file
        && let Err(e) = d.bans.save(f, d.clock.now_ns(), d.wall_offset_ms)
    {
        drop(
            d.log
                .line(edgefront_limiter::LogLevel::Error, "bans not saved")
                .str("file", &f.display().to_string())
                .str("error", &e.to_string()),
        );
    }
}

/// A report, or a 404 with its error.
fn reply(r: Result<String, String>) -> Response<Full<Bytes>> {
    match r {
        Ok(body) => text(StatusCode::OK, body),
        Err(e) => text(StatusCode::NOT_FOUND, format!("error {e}\n")),
    }
}

/// One query parameter's value, percent-decoded.
fn param(query: Option<&str>, name: &str) -> Option<String> {
    query?
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| decode(v))
}

/// Percent-decoding, leaving a malformed escape as it is.
fn decode(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(x) = v
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(x);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
                let r = answer(&d, req.method(), req.uri().path(), req.uri().query());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_are_found_and_decoded() {
        let q = Some("listener=web&addr=2001%3Adb8%3A%3A1&top=3");
        assert_eq!(param(q, "listener").as_deref(), Some("web"));
        assert_eq!(param(q, "addr").as_deref(), Some("2001:db8::1"));
        assert_eq!(param(q, "top").as_deref(), Some("3"));
        assert_eq!(param(q, "nope"), None);
        assert_eq!(param(None, "top"), None);
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%zz"), "%zz");
        assert_eq!(decode("a%2"), "a%2");
        assert_eq!(decode("%41b"), "Ab");
    }
}
