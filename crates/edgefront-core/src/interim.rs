//! Informational (1xx) responses before the final one, such as
//! `103 Early Hints` (RFC 8297), on HTTP/1.1 connections.
//!
//! hyper's HTTP/1.1 server cannot send a 1xx response itself: a service
//! returns one response, and hyper refuses a 1xx status for it. So the
//! listener puts an [`Interim`] in the extensions of every HTTP/1.1 request
//! (never HTTP/1.0, which RFC 9110 section 15.2 forbids 1xx for), and the
//! connection's stream writes what it was given ahead of hyper's own bytes:
//! hyper writes nothing on a connection between reading a request and
//! the service returning its response, so an interim response sent while
//! the request is being handled reaches the client whole and before the
//! final one. Once hyper starts writing the final response, the request's
//! [`Interim`] refuses more.
//!
//! HTTP/2 gets none: hyper's HTTP/2 server does not expose h2's
//! `send_informational`, so a request served over h2 has no [`Interim`].

use std::sync::{Arc, Mutex};
use std::task::Waker;

use http::StatusCode;
use http::header::HeaderMap;

/// The most bytes of interim responses queued for one request.
pub const MAX_INTERIM_BYTES: usize = 16 << 10;

#[derive(Debug, Default)]
struct Pending {
    bytes: Vec<u8>,
    /// The request the queue belongs to; earlier handles are refused.
    generation: u64,
    /// The final response of `generation` has started.
    closed: bool,
    /// The connection task, woken when bytes are queued.
    waker: Option<Waker>,
}

/// Sends interim responses for one request.
#[derive(Debug, Clone)]
pub struct Interim {
    pending: Arc<Mutex<Pending>>,
    generation: u64,
}

impl Interim {
    /// Queues a 1xx response with `headers` ahead of the final response.
    /// Returns false when it cannot be sent: the status is not 1xx or is
    /// 101, the final response has started, or the queue is full.
    pub fn send(&self, status: StatusCode, headers: &HeaderMap) -> bool {
        if !status.is_informational() || status == StatusCode::SWITCHING_PROTOCOLS {
            return false;
        }
        let mut head = format!(
            "HTTP/1.1 {} {}\r\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or("Informational")
        )
        .into_bytes();
        for (name, value) in headers {
            head.extend_from_slice(name.as_str().as_bytes());
            head.extend_from_slice(b": ");
            head.extend_from_slice(value.as_bytes());
            head.extend_from_slice(b"\r\n");
        }
        head.extend_from_slice(b"\r\n");
        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if p.generation != self.generation
            || p.closed
            || p.bytes.len() + head.len() > MAX_INTERIM_BYTES
        {
            return false;
        }
        p.bytes.extend_from_slice(&head);
        if let Some(w) = p.waker.take() {
            w.wake();
        }
        true
    }
}

/// A connection's side of its requests' [`Interim`]s.
#[derive(Debug, Default)]
pub(crate) struct InterimSlot {
    pending: Arc<Mutex<Pending>>,
}

impl InterimSlot {
    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The handle for a new request, which retires the previous one's.
    pub(crate) fn begin(&self) -> Interim {
        let mut p = self.lock();
        p.generation += 1;
        p.closed = false;
        p.bytes.clear();
        Interim {
            pending: self.pending.clone(),
            generation: p.generation,
        }
    }

    /// Remembers the task to wake when bytes are queued.
    pub(crate) fn register(&self, waker: &Waker) {
        let mut p = self.lock();
        if !p.waker.as_ref().is_some_and(|w| w.will_wake(waker)) {
            p.waker = Some(waker.clone());
        }
    }

    /// Writes queued bytes with `write`, which returns how many it took or
    /// `None` when it would block. True when the queue is empty after.
    pub(crate) fn drain(
        &self,
        mut write: impl FnMut(&[u8]) -> Option<std::io::Result<usize>>,
    ) -> Option<std::io::Result<()>> {
        let mut p = self.lock();
        while !p.bytes.is_empty() {
            match write(&p.bytes)? {
                Ok(0) => return Some(Err(std::io::ErrorKind::WriteZero.into())),
                Ok(n) => {
                    p.bytes.drain(..n);
                }
                Err(e) => return Some(Err(e)),
            }
        }
        Some(Ok(()))
    }

    /// The final response has started: no more interim responses for the
    /// current request.
    pub(crate) fn close(&self) {
        self.lock().closed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn link() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            "link",
            HeaderValue::from_static("</a.css>; rel=preload; as=style"),
        );
        h
    }

    fn drained(slot: &InterimSlot) -> Vec<u8> {
        let mut out = Vec::new();
        slot.drain(|b| {
            out.extend_from_slice(b);
            Some(Ok(b.len()))
        })
        .unwrap()
        .unwrap();
        out
    }

    #[test]
    fn early_hints_are_framed_and_refused_after_the_final_response() {
        let slot = InterimSlot::default();
        let first = slot.begin();
        assert!(first.send(StatusCode::from_u16(103).unwrap(), &link()));
        assert_eq!(
            drained(&slot),
            b"HTTP/1.1 103 Early Hints\r\nlink: </a.css>; rel=preload; as=style\r\n\r\n"
        );
        assert!(!first.send(StatusCode::OK, &link()));
        assert!(!first.send(StatusCode::SWITCHING_PROTOCOLS, &link()));
        slot.close();
        assert!(!first.send(StatusCode::from_u16(103).unwrap(), &link()));
        // The next request gets a fresh queue; the old handle stays refused.
        let second = slot.begin();
        assert!(second.send(StatusCode::CONTINUE, &HeaderMap::new()));
        assert!(!first.send(StatusCode::from_u16(103).unwrap(), &link()));
        assert_eq!(drained(&slot), b"HTTP/1.1 100 Continue\r\n\r\n");
    }

    #[test]
    fn the_queue_is_bounded_and_survives_short_writes() {
        let slot = InterimSlot::default();
        let i = slot.begin();
        let mut big = HeaderMap::new();
        big.insert("link", HeaderValue::from_str(&"x".repeat(4000)).unwrap());
        // Status line, the field line and the blank line.
        let one = "HTTP/1.1 103 Early Hints\r\n".len() + "link: \r\n".len() + 4000 + 2;
        let mut sent = 0;
        while i.send(StatusCode::from_u16(103).unwrap(), &big) {
            sent += 1;
        }
        assert_eq!(sent, MAX_INTERIM_BYTES / one);
        // One byte at a time, then a stall: the rest stays queued.
        let mut out = Vec::new();
        let mut budget = 10;
        assert!(
            slot.drain(|b| {
                if budget == 0 {
                    return None;
                }
                budget -= 1;
                out.push(b[0]);
                Some(Ok(1))
            })
            .is_none()
        );
        assert_eq!(out, b"HTTP/1.1 1");
        assert_eq!(drained(&slot).len(), sent * one - 10);
        assert!(slot.drain(|_| Some(Ok(0))).unwrap().is_ok(), "nothing left");
        assert!(i.send(StatusCode::from_u16(103).unwrap(), &big));
        assert!(slot.drain(|_| Some(Ok(0))).unwrap().is_err());
    }
}
