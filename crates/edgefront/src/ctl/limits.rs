//! `edgefront ctl limits`: pacerctl's commands, sent to `edgefront limits`
//! over its admin socket as HTTP requests.
//!
//! ```text
//! edgefront ctl limits [-s SOCKET] [-l LISTENER] stats|ping|reload|drain
//! edgefront ctl limits [-s SOCKET] [-l LISTENER] top [N]
//! edgefront ctl limits [-s SOCKET] [-l LISTENER] client ADDRESS
//! ```
//!
//! `top` lists each listener's N busiest clients (10 by default) with
//! their open connections and what is left of their limits, and `client`
//! one address's; `-l` keeps either to one listener.
//!
//! The answer is printed as it comes; one that starts with `error ` goes
//! to stderr and exits 1, as pacerctl did. Exit code 2 is bad usage, 1 a
//! socket that cannot be reached or does not answer within 5 seconds.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use bytes::Bytes;
use http::{Method, Request};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;

/// pacerctl's default socket.
pub const DEFAULT_SOCKET: &str = "/run/pacerd/admin.sock";

/// How long to wait for the answer: pacerctl's 5 seconds.
const TIMEOUT: Duration = Duration::from_secs(5);

const USAGE: &str = "usage: edgefront ctl limits [-s SOCKET] [-l LISTENER] COMMAND\n  commands: stats, ping, reload, drain, top [N], client ADDRESS\n  -s SOCKET    admin socket (default /run/pacerd/admin.sock)\n  -l LISTENER  only this listener, for top and client\n";

/// A query value, percent-encoded.
fn enc(v: &str) -> String {
    v.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// The request for a command and its argument, with the listener `-l`
/// named, or `None` for a command that does not exist or misses its
/// argument.
pub fn request_for(
    cmd: &str,
    arg: Option<&str>,
    listener: Option<&str>,
) -> Option<(Method, String)> {
    let only = listener.map_or(String::new(), |l| format!("&listener={}", enc(l)));
    Some(match (cmd, arg) {
        ("stats", None) => (Method::GET, "/stats".into()),
        ("ping", None) => (Method::GET, "/ping".into()),
        ("reload", None) => (Method::POST, "/reload".into()),
        ("drain", None) => (Method::POST, "/drain".into()),
        ("top", None) => (Method::GET, format!("/clients?top=10{only}")),
        ("top", Some(n)) if n.parse::<usize>().is_ok() => {
            (Method::GET, format!("/clients?top={n}{only}"))
        }
        ("client", Some(a)) => (Method::GET, format!("/client?addr={}{only}", enc(a))),
        _ => return None,
    })
}

/// Sends `method path` over the Unix socket at `socket` and returns the
/// answer's body.
pub async fn call(socket: &Path, method: Method, path: &str) -> Result<String, String> {
    let shown = socket.display();
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|e| format!("cannot connect to {shown}: {e}"))?;
    let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| format!("cannot send to {shown}: {e}"))?;
    let driver = tokio::spawn(conn);
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header(http::header::HOST, "edgefront")
        .body(Empty::<Bytes>::new())
        .map_err(|e| e.to_string())?;
    let resp = send
        .send_request(req)
        .await
        .map_err(|e| format!("cannot read from {shown}: {e}"))?;
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("cannot read from {shown}: {e}"))?
        .to_bytes();
    driver.abort();
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Runs `edgefront ctl limits` with the arguments after `limits`.
pub fn run(args: &[String]) -> ExitCode {
    let mut rest = args;
    let mut socket = DEFAULT_SOCKET;
    let mut listener = None;
    loop {
        match rest {
            [flag, v, tail @ ..] if flag == "-s" => {
                socket = v.as_str();
                rest = tail;
            }
            [flag, v, tail @ ..] if flag == "-l" => {
                listener = Some(v.as_str());
                rest = tail;
            }
            _ => break,
        }
    }
    let req = match rest {
        [cmd] => request_for(cmd, None, listener),
        [cmd, arg] => request_for(cmd, Some(arg), listener),
        _ => None,
    };
    let Some((method, path)) = req else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("edgefront: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let socket = Path::new(socket);
    let answer = rt.block_on(async {
        tokio::time::timeout(TIMEOUT, call(socket, method, &path))
            .await
            .unwrap_or_else(|_| Err(format!("no answer from {}", socket.display())))
    });
    match answer {
        Ok(body) if body.starts_with("error ") => {
            eprint!("{body}");
            ExitCode::FAILURE
        }
        Ok(body) => {
            print!("{body}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("edgefront: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_map_to_admin_requests() {
        let r = |c, a, l| request_for(c, a, l).map(|(m, p)| format!("{m} {p}"));
        assert_eq!(r("stats", None, None).as_deref(), Some("GET /stats"));
        assert_eq!(r("drain", None, None).as_deref(), Some("POST /drain"));
        assert_eq!(r("top", None, None).as_deref(), Some("GET /clients?top=10"));
        assert_eq!(
            r("top", Some("3"), Some("web 1")).as_deref(),
            Some("GET /clients?top=3&listener=web%201")
        );
        assert_eq!(
            r("client", Some("2001:db8::1"), None).as_deref(),
            Some("GET /client?addr=2001%3Adb8%3A%3A1")
        );
        assert_eq!(r("top", Some("x"), None), None);
        assert_eq!(r("client", None, None), None);
        assert_eq!(r("stats", Some("x"), None), None);
        assert_eq!(r("bogus", None, None), None);
    }
}
