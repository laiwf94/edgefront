//! `edgefront ctl limits`: pacerctl's commands, sent to `edgefront limits`
//! over its admin socket as HTTP requests.
//!
//! ```text
//! edgefront ctl limits [-s SOCKET] stats|ping|reload|drain
//! ```
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

const USAGE: &str = "usage: edgefront ctl limits [-s SOCKET] stats|ping|reload|drain\n  -s SOCKET  admin socket (default /run/pacerd/admin.sock)\n";

/// The request for a pacerctl command.
pub fn request_for(cmd: &str) -> Option<(Method, &'static str)> {
    Some(match cmd {
        "stats" => (Method::GET, "/stats"),
        "ping" => (Method::GET, "/ping"),
        "reload" => (Method::POST, "/reload"),
        "drain" => (Method::POST, "/drain"),
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
    let (socket, cmd) = match args {
        [s, path, cmd] if s == "-s" => (path.as_str(), cmd.as_str()),
        [cmd] => (DEFAULT_SOCKET, cmd.as_str()),
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let Some((method, path)) = request_for(cmd) else {
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
        tokio::time::timeout(TIMEOUT, call(socket, method, path))
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
