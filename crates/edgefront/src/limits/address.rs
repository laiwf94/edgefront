//! Address policy on the daemon's accept path: the config file's
//! `[address_policy]` section ([`edgefront_limiter::config::address_policy`]),
//! the client behind trusted proxies, and the class decision before a
//! connection is admitted.
//!
//! With no section, nothing here runs and admission is pacerd's. With
//! one, every accepted connection is decided by
//! [`AddressPolicy::decide`]:
//!
//! - with a `header`, the request head is read with `peek` (so nothing is
//!   taken from the stream and the upstream sees every byte) until its
//!   blank line, up to [`HEAD_MAX`] bytes and the listener's
//!   `connect_timeout`. A head that does not end in time, is too large or
//!   is not HTTP gives no header values, so the peer itself is the client:
//!   a trusted proxy's address, never one a client chose. At most
//!   [`HEAD_WAITS`] connections wait for their heads at once; past that a
//!   connection is refused as `listener_full`, so idle connections cannot
//!   pile up before any limit has counted them;
//! - an address no class lists gets the listener's own limits, keyed by
//!   the resolved client;
//! - `deny`, a limited class at its cap or rate, or a full class table
//!   refuses the connection with nothing read, logged as `conn rejected`
//!   with reason `class_deny`, `class_conns`, `class_rate` or
//!   `class_table_full` and the class;
//! - `allow` and an admitted limited client take only a listener slot:
//!   the class's limits replace the listener's per-client ones.

use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use edgefront_limiter::LogLevel;
use edgefront_limiter::client_addr::Header;
use edgefront_limiter::config::AddressPolicyConfig;
use edgefront_limiter::config::address_policy::Summary;
use edgefront_limiter::policy::AddressPolicy;
use tokio::net::TcpStream;
use tokio::time::Instant;

/// The largest request head read for the forwarding header.
pub const HEAD_MAX: usize = 8192;

/// Connections that may wait for their request heads at once.
pub const HEAD_WAITS: usize = 4096;

/// The `[address_policy]` section of the TOML config at `path`, built,
/// with its list files read from beside it; `None` for a pacerd.conf or
/// a TOML config without the section.
pub fn load(path: &Path) -> Result<Option<(AddressPolicy, Summary)>, String> {
    if path.extension().is_none_or(|e| e != "toml") {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    #[derive(serde::Deserialize)]
    struct Root {
        address_policy: Option<AddressPolicyConfig>,
    }
    // The limiter section beside it is read and checked on its own.
    let root: Root = toml::from_str(&text).map_err(|e| e.to_string().trim_end().to_string())?;
    let Some(cfg) = root.address_policy else {
        return Ok(None);
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    cfg.load(dir).map(Some)
}

/// The values of `header` in the request head at the start of `stream`,
/// read without consuming it. `None` when the peer closed before sending
/// a head; an empty list when the head has no such header, or is too
/// large, too slow or not HTTP.
pub async fn header_values(
    stream: &TcpStream,
    header: Header,
    wait: Duration,
) -> Option<Vec<String>> {
    let mut buf = vec![0u8; HEAD_MAX];
    let deadline = Instant::now() + wait;
    let mut pause = Duration::from_millis(1);
    loop {
        let n = match tokio::time::timeout_at(deadline, stream.peek(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return None,
            Ok(Ok(n)) => n,
            Err(_) => return Some(Vec::new()),
        };
        if let Some(end) = find(&buf[..n], b"\r\n\r\n") {
            return Some(values(&buf[..end], header));
        }
        if n == buf.len() {
            return Some(Vec::new());
        }
        // A peek returns at once while bytes are queued, so wait for more
        // to arrive before looking again.
        if Instant::now() + pause >= deadline {
            return Some(Vec::new());
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_millis(20));
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The values of `header` in a request head (without its blank line):
/// each line after the request line whose name matches, in order. A head
/// whose first line is not an HTTP request line gives none.
pub fn values(head: &[u8], header: Header) -> Vec<String> {
    let mut lines = head.split(|&b| b == b'\n');
    let Some(first) = lines.next() else {
        return Vec::new();
    };
    if !first.ends_with(b"\r") || !first.windows(5).any(|w| w == b"HTTP/") {
        return Vec::new();
    }
    let name = header.name().as_bytes();
    let mut out = Vec::new();
    for line in lines {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        if line[..colon].eq_ignore_ascii_case(name) {
            out.push(
                String::from_utf8_lossy(&line[colon + 1..])
                    .trim()
                    .to_string(),
            );
        }
    }
    out
}

/// What the daemon does with one connection after the policy decided.
#[derive(Debug)]
pub enum Plan {
    /// Admit under the listener's own limits, keyed by this client.
    Listener(IpAddr),
    /// Admit with a listener slot only, holding the class permit, if any.
    Slot(IpAddr, Option<edgefront_limiter::policy::ClassPermit>),
    /// Refuse, with the log reason and the class.
    Refuse(&'static str, String, IpAddr),
}

/// Decides `peer`'s connection on `stream` under `policy`, waiting at
/// most `wait` for the request head when a header is configured. `None`
/// when the peer closed first.
pub async fn decide(
    policy: &Arc<AddressPolicy>,
    stream: &TcpStream,
    peer: IpAddr,
    wait: Duration,
    now_ns: impl Fn() -> u64,
    log: &super::log::Log,
) -> Option<Plan> {
    let values = match policy.header() {
        Some(h) => header_values(stream, h, wait).await?,
        None => Vec::new(),
    };
    let refs: Vec<&str> = values.iter().map(String::as_str).collect();
    let d = policy.decide(peer, &refs, now_ns());
    if let Some(why) = &d.resolved.malformed {
        drop(
            log.line(LogLevel::Debug, "malformed forwarding hop")
                .str("peer", &peer.to_string())
                .str("client", &d.resolved.client.to_string())
                .str("error", why),
        );
    }
    use edgefront_limiter::policy::Verdict;
    let client = d.resolved.client;
    let name = d.verdict.name();
    Some(match d.verdict {
        Verdict::Default => Plan::Listener(client),
        Verdict::Allow => Plan::Slot(client, None),
        Verdict::Admitted(p) => Plan::Slot(client, Some(p)),
        Verdict::Deny | Verdict::OverCap | Verdict::OverRate(_) | Verdict::TableFull => {
            Plan::Refuse(name, d.class.unwrap_or_default(), client)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_values_come_from_an_http_head_only() {
        let head = b"GET / HTTP/1.1\r\nHost: a\r\nX-Forwarded-For: 203.0.113.5\r\nx-forwarded-for:  192.0.2.7 \r\nForwarded: for=1.2.3.4";
        assert_eq!(
            values(head, Header::XForwardedFor),
            ["203.0.113.5", "192.0.2.7"]
        );
        assert_eq!(values(head, Header::Forwarded), ["for=1.2.3.4"]);
        assert!(
            values(
                b"SSH-2.0-OpenSSH\r\nX-Forwarded-For: 1.2.3.4",
                Header::XForwardedFor
            )
            .is_empty()
        );
        assert!(values(b"", Header::XForwardedFor).is_empty());
        assert!(values(b"GET / HTTP/1.1", Header::XForwardedFor).is_empty());
        assert_eq!(find(b"ab\r\n\r\ncd", b"\r\n\r\n"), Some(2));
        assert_eq!(find(b"ab", b"\r\n\r\n"), None);
    }

    #[tokio::test]
    async fn the_head_is_peeked_not_taken_and_bounded_in_time_and_size() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        // A head sent in two parts is read whole, and stays in the stream.
        let mut c = TcpStream::connect(addr).await.unwrap();
        let (s, _) = l.accept().await.unwrap();
        c.write_all(b"GET / HTTP/1.1\r\nX-Forwarded-For: 203.0.113.5\r\n")
            .await
            .unwrap();
        let w = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            c.write_all(b"\r\n").await.unwrap();
            c
        });
        let got = header_values(&s, Header::XForwardedFor, Duration::from_secs(5)).await;
        assert_eq!(got.unwrap(), ["203.0.113.5"]);
        let _c = w.await.unwrap();
        let mut s = s;
        let mut buf = [0u8; 64];
        let n = s.read(&mut buf).await.unwrap();
        assert!(buf[..n].starts_with(b"GET / HTTP/1.1\r\n"));
        // No blank line in time: no values.
        let mut c = TcpStream::connect(addr).await.unwrap();
        let (s, _) = l.accept().await.unwrap();
        c.write_all(b"GET / HTTP/1.1\r\nX-Forwarded-For: 1.2.3.4\r\n")
            .await
            .unwrap();
        let t = std::time::Instant::now();
        let got = header_values(&s, Header::XForwardedFor, Duration::from_millis(200)).await;
        assert_eq!(got.unwrap(), Vec::<String>::new());
        assert!(t.elapsed() < Duration::from_secs(2));
        // Too large: no values.
        let mut c2 = TcpStream::connect(addr).await.unwrap();
        let (s2, _) = l.accept().await.unwrap();
        let mut big = b"GET / HTTP/1.1\r\nX-Forwarded-For: 1.2.3.4\r\n".to_vec();
        big.resize(HEAD_MAX + 100, b'a');
        c2.write_all(&big).await.unwrap();
        let got = header_values(&s2, Header::XForwardedFor, Duration::from_secs(5)).await;
        assert_eq!(got.unwrap(), Vec::<String>::new());
        // Closed before a head: None.
        drop(c);
        let c3 = TcpStream::connect(addr).await.unwrap();
        let (s3, _) = l.accept().await.unwrap();
        drop(c3);
        assert_eq!(
            header_values(&s3, Header::XForwardedFor, Duration::from_secs(5)).await,
            None
        );
    }
}
