//! Streaming fill: one origin body, read by every client that waits on it
//! while it is still arriving.
//!
//! A [`Filling`] is a growing list of the body's chunks. The fill pushes
//! each chunk as it arrives from the origin; each [`FillReader`] is an HTTP
//! body that yields the chunks in order, from the first, and waits for the
//! next one when it has caught up. A client that joins halfway through
//! gets what has already arrived at once and the rest as it comes. When
//! the body is complete the fill stores the object and [`Filling::finish`]
//! ends every reader's body; when the origin fails partway,
//! [`Filling::abort`] fails every reader's body with the reason, so each
//! client sees a broken response rather than a short one taken for whole.

use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

use bytes::{Bytes, BytesMut};
use hyper::body::{Body, Frame, SizeHint};
use tokio::sync::watch;

use crate::object::CachedObject;
use crate::respond::BoxError;

/// How a fill ended.
#[derive(Clone, Debug)]
pub enum End {
    /// Every byte arrived; the object, if it was stored.
    Complete(Option<Arc<CachedObject>>),
    /// The origin failed partway, for this reason.
    Aborted(Arc<str>),
}

impl PartialEq for End {
    /// The same ending, and for a stored object the same one.
    fn eq(&self, other: &End) -> bool {
        match (self, other) {
            (End::Complete(a), End::Complete(b)) => match (a, b) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            },
            (End::Aborted(a), End::Aborted(b)) => a == b,
            _ => false,
        }
    }
}

#[derive(Default)]
struct State {
    chunks: Vec<Bytes>,
    len: u64,
    end: Option<End>,
    wakers: Vec<Waker>,
}

/// A body being filled from the origin.
pub struct Filling {
    state: Mutex<State>,
    /// The declared length, if the origin gave one.
    expected: Option<u64>,
    done: watch::Sender<Option<End>>,
}

/// The error a reader's body ends with when its fill is aborted.
#[derive(Debug)]
pub struct Aborted(pub Arc<str>);

impl std::fmt::Display for Aborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the origin response failed partway: {}", self.0)
    }
}

impl std::error::Error for Aborted {}

impl Filling {
    /// An empty fill of a body declared as `expected` bytes, if known.
    pub fn new(expected: Option<u64>) -> Arc<Filling> {
        let (done, _) = watch::channel(None);
        Arc::new(Filling {
            state: Mutex::new(State::default()),
            expected,
            done,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wake(state: &mut State) {
        for waker in state.wakers.drain(..) {
            waker.wake();
        }
    }

    /// Appends a chunk. Ignored once the fill has ended.
    pub fn push(&self, chunk: Bytes) {
        if chunk.is_empty() {
            return;
        }
        let mut state = self.lock();
        if state.end.is_some() {
            return;
        }
        state.len += chunk.len() as u64;
        state.chunks.push(chunk);
        Self::wake(&mut state);
    }

    fn end(&self, end: End) {
        let mut state = self.lock();
        if state.end.is_some() {
            return;
        }
        state.end = Some(end.clone());
        Self::wake(&mut state);
        drop(state);
        self.done.send_replace(Some(end));
    }

    /// Ends every reader's body: all the bytes have arrived. `stored` is
    /// the object the fill stored, if it did.
    pub fn finish(&self, stored: Option<Arc<CachedObject>>) {
        self.end(End::Complete(stored));
    }

    /// Fails every reader's body with `reason`.
    pub fn abort(&self, reason: &str) {
        self.end(End::Aborted(reason.into()));
    }

    /// The declared length, if the origin gave one.
    pub fn expected(&self) -> Option<u64> {
        self.expected
    }

    /// The bytes received so far.
    pub fn received(&self) -> u64 {
        self.lock().len
    }

    /// The whole body, once every byte has arrived (before
    /// [`Filling::finish`], so the fill can store it).
    pub fn collect(&self) -> Bytes {
        let state = self.lock();
        if let [one] = state.chunks.as_slice() {
            return one.clone();
        }
        let mut out = BytesMut::with_capacity(state.len as usize);
        for chunk in &state.chunks {
            out.extend_from_slice(chunk);
        }
        out.freeze()
    }

    /// Waits for the fill to end.
    pub async fn ended(&self) -> End {
        let mut rx = self.done.subscribe();
        match rx.wait_for(Option::is_some).await {
            Ok(end) => end.clone().expect("waited for Some"),
            // The sender lives as long as self, so this cannot happen.
            Err(_) => End::Aborted("fill dropped".into()),
        }
    }

    /// A body that reads this fill from its first byte.
    pub fn reader(self: &Arc<Self>) -> FillReader {
        FillReader {
            fill: Arc::clone(self),
            next: 0,
            sent: 0,
        }
    }
}

/// One client's view of a [`Filling`].
pub struct FillReader {
    fill: Arc<Filling>,
    next: usize,
    sent: u64,
}

impl Body for FillReader {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let fill = Arc::clone(&self.fill);
        let mut state = fill.lock();
        if let Some(chunk) = state.chunks.get(self.next).cloned() {
            drop(state);
            self.next += 1;
            self.sent += chunk.len() as u64;
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }
        match &state.end {
            Some(End::Complete(_)) => Poll::Ready(None),
            Some(End::Aborted(reason)) => Poll::Ready(Some(Err(Box::new(Aborted(reason.clone()))))),
            None => {
                state.wakers.push(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self.fill.expected {
            Some(n) => SizeHint::with_exact(n.saturating_sub(self.sent)),
            None => SizeHint::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use http_body_util::BodyExt;

    use super::*;

    #[tokio::test]
    async fn a_late_reader_gets_everything_from_the_start() {
        let fill = Filling::new(Some(9));
        assert_eq!(fill.expected(), Some(9));
        let mut early = fill.reader();
        fill.push(Bytes::from_static(b"abc"));
        let first = early.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(first, "abc");
        assert_eq!(early.size_hint().exact(), Some(6));
        fill.push(Bytes::from_static(b"def"));
        let late = fill.reader();
        fill.push(Bytes::new());
        fill.push(Bytes::from_static(b"ghi"));
        assert_eq!(fill.received(), 9);
        assert_eq!(fill.collect(), "abcdefghi");
        fill.finish(None);
        // Ignored after the end.
        fill.push(Bytes::from_static(b"zzz"));
        fill.abort("late");
        assert_eq!(late.collect().await.unwrap().to_bytes(), "abcdefghi");
        assert_eq!(early.collect().await.unwrap().to_bytes(), "defghi");
        assert_eq!(fill.ended().await, End::Complete(None));
    }

    #[tokio::test]
    async fn a_waiting_reader_wakes_on_each_chunk() {
        let fill = Filling::new(None);
        let reader = fill.reader();
        assert_eq!(reader.size_hint().exact(), None);
        let writer = Arc::clone(&fill);
        tokio::spawn(async move {
            for part in ["one ", "two ", "three"] {
                tokio::time::sleep(Duration::from_millis(5)).await;
                writer.push(Bytes::from(part));
            }
            writer.finish(None);
        });
        assert_eq!(reader.collect().await.unwrap().to_bytes(), "one two three");
    }

    #[tokio::test]
    async fn an_abort_fails_every_reader() {
        let fill = Filling::new(Some(100));
        let a = fill.reader();
        fill.push(Bytes::from_static(b"partial"));
        let b = fill.reader();
        let ended = {
            let fill = Arc::clone(&fill);
            tokio::spawn(async move { fill.ended().await })
        };
        fill.abort("reset");
        for reader in [a, b] {
            let err = reader.collect().await.err().unwrap();
            assert!(err.to_string().contains("failed partway: reset"), "{err}");
        }
        assert_eq!(ended.await.unwrap(), End::Aborted("reset".into()));
        // A single chunk is handed back without a copy.
        let one = Filling::new(None);
        one.push(Bytes::from_static(b"x"));
        assert_eq!(one.collect(), "x");
    }
}
