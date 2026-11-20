//! Large objects cached as slices.
//!
//! A response larger than the route's `max_object_bytes` used to stream
//! through uncached. With `slice_bytes` set (1 MiB by default) it is cut
//! into slices of that size as it arrives, and each slice is stored as an
//! object of its own under [`slice_key`]. Once every slice is in, a
//! *manifest* is stored under the object's own key: its status and headers,
//! no body, and an internal [`SLICES`] field giving the slice size and the
//! total length. A hit on a manifest is answered by reading the slices in
//! order, for the whole object or one range of it.
//!
//! Every slice carries the object's surrogate keys and one of its own,
//! [`slice_tag`], so a purge by tag or by URL removes the slices with the
//! manifest. A slice fetched later with `Range` (because the slices a client
//! needs are no longer all held) is checked against the manifest's entity
//! tag: a different tag means the object changed at the origin, and the old
//! slices and manifest are dropped before the new ones are stored.
//!
//! Every client of a slice fill, the one that started it included, reads
//! through a [`Progress`]: the slices already stored come from the cache,
//! and the one still arriving from its [`Filling`], so a client that joins a
//! 2 GiB fill halfway gets its first byte at once.

use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use http::header::{HeaderMap, HeaderName, HeaderValue};
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, SizeHint};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};

use crate::fill::{End, Filling};
use crate::headers::EntityTag;
use crate::object::{CachedObject, Key, SURROGATE_KEY};
use crate::respond::BoxError;
use crate::store::Cache;

/// The internal field on a manifest: `<slice bytes> <total bytes>`. It is
/// never sent to a client.
pub const SLICES: HeaderName = HeaderName::from_static("edgefront-slices");

/// The default slice size.
pub const DEFAULT_SLICE_BYTES: u64 = 1 << 20;

/// The slice size a route gets from its `slice_bytes` and object limit: at
/// most half the limit, so a slice with its headers is always storable, and
/// 0 (slicing off) when that leaves less than 64 KiB.
pub fn effective(slice_bytes: u64, max_object_bytes: u64) -> u64 {
    let size = slice_bytes.min(max_object_bytes / 2);
    if size < 64 << 10 { 0 } else { size }
}

/// The key slice `index` of `key` is stored under.
pub fn slice_key(key: &str, index: u64) -> Key {
    format!("{key}\u{1f}slice={index}").into()
}

/// The surrogate key every slice of `key` carries: a hash, so any key
/// makes a valid, short tag.
pub fn slice_tag(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let mut tag = String::from("edgefront-slices-");
    for b in &digest[..12] {
        tag.push_str(&format!("{b:02x}"));
    }
    tag
}

/// The slice size and total length a manifest records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub slice_bytes: u64,
    pub total: u64,
}

impl Layout {
    /// The layout a stored object records, if it is a manifest.
    pub fn of(obj: &CachedObject) -> Option<Layout> {
        let text = obj.headers.get(&SLICES)?.to_str().ok()?;
        let (size, total) = text.split_once(' ')?;
        let layout = Layout {
            slice_bytes: size.parse().ok()?,
            total: total.parse().ok()?,
        };
        (layout.slice_bytes > 0).then_some(layout)
    }

    /// The field value recording this layout.
    pub fn header(&self) -> HeaderValue {
        HeaderValue::from_str(&format!("{} {}", self.slice_bytes, self.total))
            .expect("digits and a space are a valid field value")
    }

    /// How many slices there are.
    pub fn count(&self) -> u64 {
        self.total.div_ceil(self.slice_bytes)
    }

    /// The slices holding bytes `start..=end`.
    pub fn covering(&self, start: u64, end: u64) -> std::ops::RangeInclusive<u64> {
        start / self.slice_bytes..=end / self.slice_bytes
    }

    /// The first byte of slice `index`.
    pub fn start_of(&self, index: u64) -> u64 {
        index * self.slice_bytes
    }

    /// The length of slice `index`.
    pub fn len_of(&self, index: u64) -> u64 {
        (self.total - self.start_of(index)).min(self.slice_bytes)
    }
}

/// The headers a slice is stored with: the object's, without framing or
/// the manifest field, with the slice tag added to its surrogate keys.
pub fn slice_headers(headers: &HeaderMap, key: &str) -> HeaderMap {
    let mut out = headers.clone();
    out.remove(http::header::CONTENT_LENGTH);
    out.remove(http::header::CONTENT_RANGE);
    out.remove(&SLICES);
    let tag = slice_tag(key);
    let keys = match out.get(SURROGATE_KEY).and_then(|v| v.to_str().ok()) {
        Some(existing) => format!("{existing} {tag}"),
        None => tag,
    };
    if let Ok(value) = HeaderValue::from_str(&keys) {
        out.insert(SURROGATE_KEY, value);
    }
    out
}

/// How many completed slices a fill keeps for readers that lag behind it,
/// so that one whose slice has already left the memory tier (and is still
/// queued for disk) is not cut off.
pub const RECENT: usize = 64;

#[derive(Default)]
struct State {
    /// The last [`RECENT`] completed slices.
    recent: std::collections::VecDeque<(u64, Bytes)>,
    /// Slices stored so far, from the first.
    stored: u64,
    /// The slice arriving now and its index.
    current: Option<(u64, Arc<Filling>)>,
    end: Option<End>,
}

/// A slice fill in progress, as its readers see it.
pub struct Progress {
    pub layout: Layout,
    /// The first slice this fill writes.
    pub first: u64,
    /// The last byte it writes.
    pub last_byte: u64,
    state: Mutex<State>,
    changed: watch::Sender<u64>,
}

impl Progress {
    /// A fill writing `layout`'s slices from slice `first` to byte
    /// `last_byte`.
    pub fn new(layout: Layout, first: u64, last_byte: u64) -> Arc<Progress> {
        let (changed, _) = watch::channel(0);
        Arc::new(Progress {
            layout,
            first,
            last_byte,
            state: Mutex::new(State {
                stored: first,
                ..State::default()
            }),
            changed,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n += 1);
    }

    /// Slice `index` starts arriving into `fill`.
    pub fn begin(&self, index: u64, fill: Arc<Filling>) {
        self.lock().current = Some((index, fill));
        self.bump();
    }

    /// Slice `index`, `bytes`, is complete and stored (or could not be).
    pub fn stored(&self, index: u64, bytes: Bytes) {
        let mut state = self.lock();
        state.stored = index + 1;
        if state.recent.len() == RECENT {
            state.recent.pop_front();
        }
        state.recent.push_back((index, bytes));
        if state.current.as_ref().is_some_and(|(i, _)| *i == index) {
            state.current = None;
        }
        drop(state);
        self.bump();
    }

    /// Whether this fill writes every byte of `start..=end`.
    pub fn covers(&self, start: u64, end: u64) -> bool {
        start >= self.layout.start_of(self.first) && end <= self.last_byte
    }

    /// The fill is over.
    pub fn end(&self, end: End) {
        let mut state = self.lock();
        if let Some((_, fill)) = state.current.take() {
            match &end {
                End::Aborted(why) => fill.abort(why),
                End::Complete(_) => fill.finish(None),
            }
        }
        state.end = Some(end);
        drop(state);
        self.bump();
    }
}

/// A body fed from a channel by a task, of a known length.
pub struct ChannelBody {
    rx: mpsc::Receiver<Result<Bytes, BoxError>>,
    remaining: u64,
}

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                self.remaining = self.remaining.saturating_sub(bytes.len() as u64);
                Poll::Ready(Some(Ok(Frame::data(bytes))))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) if self.remaining > 0 => Poll::Ready(Some(Err(
                "the slices of the response ran out before its end".into(),
            ))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

/// A body of bytes `start..=end` of the sliced object at `key`, read slice
/// by slice from the cache, and from `progress` for the slices a fill is
/// still writing. A slice that is missing when its turn comes, or whose
/// entity tag is not the strong `etag` of the manifest, fails the body.
pub fn read(
    cache: Arc<Cache>,
    key: Key,
    layout: Layout,
    start: u64,
    end: u64,
    etag: Option<EntityTag>,
    progress: Option<Arc<Progress>>,
) -> ChannelBody {
    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(async move {
        let result = feed(
            &cache,
            &key,
            layout,
            start,
            end,
            etag.as_ref(),
            progress.as_deref(),
            &tx,
        )
        .await;
        if let Err(e) = result {
            let _ = tx.send(Err(e)).await;
        }
    });
    ChannelBody {
        rx,
        remaining: end - start + 1,
    }
}

#[allow(clippy::too_many_arguments)]
async fn feed(
    cache: &Cache,
    key: &str,
    layout: Layout,
    start: u64,
    end: u64,
    etag: Option<&EntityTag>,
    progress: Option<&Progress>,
    tx: &mpsc::Sender<Result<Bytes, BoxError>>,
) -> Result<(), BoxError> {
    let mut changes = progress.map(|p| p.changed.subscribe());
    for index in layout.covering(start, end) {
        let from = layout.start_of(index);
        let lo = start.saturating_sub(from);
        let hi = (end - from).min(layout.len_of(index) - 1);
        // Wait until this slice is stored, or is the one arriving.
        let live = loop {
            let Some(p) = progress else { break None };
            if index < p.first {
                break None;
            }
            let (stored, current, end) = {
                let state = p.lock();
                (state.stored, state.current.clone(), state.end.clone())
            };
            if index < stored {
                break None;
            }
            if let Some((i, fill)) = current
                && i == index
            {
                break Some(fill);
            }
            if let Some(End::Aborted(why)) = end {
                return Err(Box::new(crate::fill::Aborted(why)));
            }
            if end.is_some() {
                break None;
            }
            if let Some(rx) = changes.as_mut()
                && rx.changed().await.is_err()
            {
                return Err("the slice fill went away".into());
            }
        };
        match live {
            Some(fill) => {
                // Stream the arriving slice from the start, skipping to `lo`.
                let mut reader = fill.reader();
                let mut at = 0u64;
                while let Some(frame) = reader.frame().await {
                    let data = frame?.into_data().unwrap_or_default();
                    let len = data.len() as u64;
                    let (a, b) = (at, at + len);
                    at = b;
                    if b <= lo || a > hi {
                        continue;
                    }
                    let s = lo.saturating_sub(a) as usize;
                    let e = ((hi + 1).min(b) - a) as usize;
                    if tx.send(Ok(data.slice(s..e))).await.is_err() {
                        return Ok(());
                    }
                    if b > hi {
                        break;
                    }
                }
            }
            None => {
                let recent = progress.and_then(|p| {
                    p.lock()
                        .recent
                        .iter()
                        .find(|(i, _)| *i == index)
                        .map(|(_, b)| b.clone())
                });
                if let Some(bytes) = recent {
                    if (bytes.len() as u64) < hi + 1 {
                        return Err("a slice is shorter than its place".into());
                    }
                    if tx
                        .send(Ok(bytes.slice(lo as usize..=hi as usize)))
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                    continue;
                }
                let slice_key = slice_key(key, index);
                let mut slice = cache.lookup(&slice_key, &HeaderMap::new()).await;
                if slice.is_none() {
                    // Out of memory but perhaps still queued for disk: wait
                    // for the queue, then look again.
                    cache.flush().await;
                    slice = cache.lookup(&slice_key, &HeaderMap::new()).await;
                }
                let slice = slice.ok_or("a slice of the object is no longer cached")?;
                // Every slice must be of the same representation as the
                // manifest: an object that changed at the origin never
                // mixes old and new bytes.
                if etag.is_some_and(|e| !slice.etag.as_ref().is_some_and(|s| s.strong_eq(e))) {
                    return Err("a cached slice is of another version of the object".into());
                }
                let body = &slice.body;
                if (body.len() as u64) < hi + 1 {
                    return Err("a cached slice is shorter than its place".into());
                }
                if tx
                    .send(Ok(body.slice(lo as usize..=hi as usize)))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// Cuts a byte stream into slices: [`Slicer::push`] returns each slice as
/// it completes.
pub struct Slicer {
    slice_bytes: usize,
    buf: BytesMut,
}

impl Slicer {
    pub fn new(slice_bytes: u64) -> Slicer {
        Slicer {
            slice_bytes: slice_bytes as usize,
            buf: BytesMut::new(),
        }
    }

    /// Adds `data` and returns the slices it completes, and the part of
    /// `data` that went into the slice still open (for its readers).
    pub fn push(&mut self, mut data: Bytes) -> Vec<(Bytes, Bytes)> {
        let mut out = Vec::new();
        while !data.is_empty() {
            let room = self.slice_bytes - self.buf.len();
            let take = room.min(data.len());
            let part = data.split_to(take);
            self.buf.extend_from_slice(&part);
            if self.buf.len() == self.slice_bytes {
                out.push((part, self.buf.split().freeze()));
            } else {
                out.push((part, Bytes::new()));
            }
        }
        out
    }

    /// The last, partial slice, if any bytes are left.
    pub fn finish(&mut self) -> Option<Bytes> {
        (!self.buf.is_empty()).then(|| self.buf.split().freeze())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_cover_ranges() {
        let l = Layout {
            slice_bytes: 10,
            total: 35,
        };
        assert_eq!(l.count(), 4);
        assert_eq!(l.covering(0, 34), 0..=3);
        assert_eq!(l.covering(9, 10), 0..=1);
        assert_eq!(l.len_of(3), 5);
        assert_eq!(l.start_of(2), 20);
        let mut h = HeaderMap::new();
        h.insert(SLICES, l.header());
        let obj = CachedObject::new(
            http::StatusCode::OK,
            h,
            Bytes::new(),
            crate::policy::Freshness {
                lifetime: std::time::Duration::from_secs(1),
                corrected_initial_age: std::time::Duration::ZERO,
                response_time: std::time::SystemTime::UNIX_EPOCH,
            },
            &HeaderMap::new(),
        );
        assert_eq!(Layout::of(&obj), Some(l));
    }

    #[test]
    fn the_effective_size_fits_the_object_limit() {
        assert_eq!(effective(1 << 20, 8 << 20), 1 << 20);
        assert_eq!(effective(1 << 20, 1 << 20), 512 << 10);
        assert_eq!(effective(1 << 20, 100 << 10), 0);
        assert_eq!(effective(0, 8 << 20), 0);
    }

    #[test]
    fn slice_keys_and_tags() {
        assert_ne!(slice_key("h/a", 0), slice_key("h/a", 1));
        let tag = slice_tag(&"x".repeat(5000));
        assert!(tag.len() < 60 && tag.starts_with("edgefront-slices-"));
        let mut h = HeaderMap::new();
        h.insert(SURROGATE_KEY, HeaderValue::from_static("video"));
        h.insert(http::header::CONTENT_LENGTH, HeaderValue::from_static("5"));
        h.insert(SLICES, HeaderValue::from_static("1 5"));
        let out = slice_headers(&h, "h/a");
        assert_eq!(out[SURROGATE_KEY], format!("video {}", slice_tag("h/a")));
        assert!(out.get(http::header::CONTENT_LENGTH).is_none() && out.get(&SLICES).is_none());
        let bare = slice_headers(&HeaderMap::new(), "h/a");
        assert_eq!(bare[SURROGATE_KEY], slice_tag("h/a"));
    }

    #[test]
    fn the_slicer_cuts_at_slice_boundaries() {
        let mut s = Slicer::new(4);
        let out = s.push(Bytes::from_static(b"abcdefghij"));
        let done: Vec<_> = out
            .iter()
            .filter(|(_, d)| !d.is_empty())
            .map(|(_, d)| d.clone())
            .collect();
        assert_eq!(done, ["abcd", "efgh"]);
        let parts: Vec<_> = out.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(parts, ["abcd", "efgh", "ij"]);
        assert!(
            s.push(Bytes::from_static(b"kl"))
                .iter()
                .any(|(_, d)| d == "ijkl")
        );
        assert_eq!(s.finish(), None);
        s.push(Bytes::from_static(b"m"));
        assert_eq!(s.finish().unwrap(), "m");
    }
}
