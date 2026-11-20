//! A versioned delta feed of endpoints over HTTP/2, with resume tokens: a
//! control plane publishes changes to a [`Feed`], [`serve`] streams them,
//! and a [`DeltaSource`] follows the stream, picking up where it left off
//! after a cut.
//!
//! ## The protocol
//!
//! The client sends `GET <url>` (HTTP/2: prior knowledge on `http://`,
//! `h2` over ALPN on `https://`), with `?since=<token>` once it has one.
//! The answer is `200` with `content-type: application/x-ndjson`: one JSON
//! object per line, until either side closes.
//!
//! - `{"type":"snapshot","feed":"<id>","version":7,"endpoints":[...]}`, the
//!   whole set, first on a stream without a usable token;
//! - `{"type":"resume","feed":"<id>","version":7}`, first on a stream whose
//!   token the feed can continue from: the changes after version 7 follow;
//! - `{"type":"upsert","version":8,"endpoint":{...}}` and
//!   `{"type":"remove","version":9,"address":"10.0.0.1:80"}`, one change
//!   each, versions rising by exactly one;
//! - `{"type":"heartbeat","version":9}` every few seconds when nothing
//!   changes, with the version the stream has reached.
//!
//! Endpoints are written as in endpoint files (`address`, and optionally
//! `weight`, `priority`, `zone` and `ready`) and checked the same way.
//!
//! The token is `<feed id>.<version>`. The feed id is random per [`Feed`],
//! so a restarted control plane that lost its history is never asked to
//! continue one it does not have. A token from another feed, from the
//! future, or older than the changes the feed keeps gets a snapshot.
//!
//! ## The client
//!
//! A snapshot becomes a whole set, and each change an upsert or a
//! removal, so the merger applies deltas without full reloads. Any break
//! (the stream ends, a connect fails, nothing arrives for three heartbeat
//! intervals) is sent as a failure, so the merger keeps the last good set,
//! and the client reconnects with its token after 100 ms, doubling to
//! 10 s. A protocol error (a skipped or repeated version, a line that does
//! not parse or fails its checks) also drops the token, so the next
//! stream starts with a snapshot. A line may be up to 64 MiB, enough for a
//! snapshot of 200,000 endpoints. A bearer token, read from `token_file`
//! for each connection, is sent only over `https://`.

use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use edgefront_core::config::UpstreamTls;
use edgefront_core::http1::TlsTo;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Empty};
use hyper::body::{Body, Frame, Incoming};
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

use super::file::WireEndpoint;
use super::{Endpoint, Source, UpdateKind};

/// The longest line read: a snapshot of 200,000 endpoints fits.
pub const MAX_LINE_BYTES: usize = 64 << 20;

/// How often a quiet stream carries a heartbeat.
pub const HEARTBEAT: Duration = Duration::from_secs(5);

/// One line on the wire, as written.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Out<'a> {
    Snapshot {
        feed: &'a str,
        version: u64,
        endpoints: &'a [Endpoint],
    },
    Resume {
        feed: &'a str,
        version: u64,
    },
    Upsert {
        version: u64,
        endpoint: &'a Endpoint,
    },
    Remove {
        version: u64,
        address: &'a str,
    },
    Heartbeat {
        version: u64,
    },
}

/// One line on the wire, as read.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum In {
    Snapshot {
        feed: String,
        version: u64,
        endpoints: Vec<WireEndpoint>,
    },
    Resume {
        feed: String,
        version: u64,
    },
    Upsert {
        version: u64,
        endpoint: WireEndpoint,
    },
    Remove {
        version: u64,
        address: String,
    },
    Heartbeat {
        version: u64,
    },
}

#[derive(Debug, Clone)]
enum Delta {
    Upsert(Endpoint),
    Remove(String),
}

struct FeedState {
    version: u64,
    endpoints: BTreeMap<String, Endpoint>,
    /// The latest changes, oldest first, with their versions.
    log: VecDeque<(u64, Delta)>,
}

/// The publishing side: the current set, a version that rises by one per
/// change, and the latest changes for streams to resume from.
pub struct Feed {
    id: String,
    keep: usize,
    state: Mutex<FeedState>,
    tick: watch::Sender<u64>,
    snapshots: AtomicU64,
    resumes: AtomicU64,
}

impl std::fmt::Debug for Feed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feed")
            .field("id", &self.id)
            .field("version", &self.version())
            .finish_non_exhaustive()
    }
}

/// What a stream starts with.
enum Start {
    Snapshot(u64, Vec<Endpoint>),
    Resume(u64),
}

impl Feed {
    /// An empty feed at version 0 that keeps its latest `keep` changes for
    /// streams to resume from.
    pub fn new(keep: usize) -> Arc<Feed> {
        let mut id = [0u8; 8];
        // Any value will do as long as two feeds differ; the balancer's
        // generator is seeded from the OS's entropy.
        let rng = crate::balancer::rng::SplitMix64::from_entropy();
        id.copy_from_slice(&rng.next_u64().to_be_bytes());
        Arc::new(Feed {
            id: id.iter().map(|b| format!("{b:02x}")).collect(),
            keep: keep.max(1),
            state: Mutex::new(FeedState {
                version: 0,
                endpoints: BTreeMap::new(),
                log: VecDeque::new(),
            }),
            tick: watch::Sender::new(0),
            snapshots: AtomicU64::new(0),
            resumes: AtomicU64::new(0),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FeedState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The feed's id, the first part of its tokens.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The version of the latest change.
    pub fn version(&self) -> u64 {
        self.lock().version
    }

    /// The current set, in address order.
    pub fn endpoints(&self) -> Vec<Endpoint> {
        self.lock().endpoints.values().cloned().collect()
    }

    /// How many streams started with a snapshot, and how many resumed.
    pub fn starts(&self) -> (u64, u64) {
        (
            self.snapshots.load(Ordering::Relaxed),
            self.resumes.load(Ordering::Relaxed),
        )
    }

    fn record(&self, s: &mut FeedState, d: Delta) {
        s.version += 1;
        s.log.push_back((s.version, d));
        while s.log.len() > self.keep {
            s.log.pop_front();
        }
        self.tick.send_replace(s.version);
    }

    /// Adds or changes `ep`, returning the version; nothing changes, and
    /// the version stays, when it is already there as it is.
    pub fn upsert(&self, ep: Endpoint) -> u64 {
        let mut s = self.lock();
        if s.endpoints.get(&ep.address) != Some(&ep) {
            s.endpoints.insert(ep.address.clone(), ep.clone());
            self.record(&mut s, Delta::Upsert(ep));
        }
        s.version
    }

    /// Removes the endpoint at `address`, returning the version.
    pub fn remove(&self, address: &str) -> u64 {
        let mut s = self.lock();
        if s.endpoints.remove(address).is_some() {
            self.record(&mut s, Delta::Remove(address.to_owned()));
        }
        s.version
    }

    /// Makes `eps` the set, as the changes that get there.
    pub fn set(&self, eps: Vec<Endpoint>) -> u64 {
        let want: BTreeMap<String, Endpoint> =
            eps.into_iter().map(|e| (e.address.clone(), e)).collect();
        let mut s = self.lock();
        let gone: Vec<String> = s
            .endpoints
            .keys()
            .filter(|a| !want.contains_key(*a))
            .cloned()
            .collect();
        for a in gone {
            s.endpoints.remove(&a);
            self.record(&mut s, Delta::Remove(a));
        }
        for (a, e) in want {
            if s.endpoints.get(&a) != Some(&e) {
                s.endpoints.insert(a, e.clone());
                self.record(&mut s, Delta::Upsert(e));
            }
        }
        s.version
    }

    /// Where a stream with `since` (a token, or none) starts.
    fn start(&self, since: Option<&str>) -> Start {
        let s = self.lock();
        let resumable = since
            .and_then(|t| t.split_once('.'))
            .filter(|(id, _)| *id == self.id)
            .and_then(|(_, v)| v.parse::<u64>().ok())
            .filter(|v| {
                let oldest = s.log.front().map_or(s.version, |(o, _)| o - 1);
                *v <= s.version && *v >= oldest
            });
        match resumable {
            Some(v) => {
                self.resumes.fetch_add(1, Ordering::Relaxed);
                Start::Resume(v)
            }
            None => {
                self.snapshots.fetch_add(1, Ordering::Relaxed);
                Start::Snapshot(s.version, s.endpoints.values().cloned().collect())
            }
        }
    }

    /// The changes after `after`, or `None` when some of them are no
    /// longer kept.
    fn after(&self, after: u64) -> Option<Vec<(u64, Delta)>> {
        let s = self.lock();
        if after >= s.version {
            return Some(Vec::new());
        }
        let first = s.log.front()?.0;
        if after + 1 < first {
            return None;
        }
        let skip = usize::try_from(after + 1 - first).ok()?;
        Some(s.log.iter().skip(skip).cloned().collect())
    }
}

fn line(out: &Out<'_>) -> Bytes {
    let mut v = serde_json::to_vec(out).expect("endpoints serialize");
    v.push(b'\n');
    Bytes::from(v)
}

/// Streams `feed` to every HTTP/2 client that connects to `listener`
/// (cleartext, prior knowledge), with a heartbeat every `heartbeat`,
/// until `shutdown` resolves. Dropping the future closes every stream,
/// as a crashed control plane would.
pub async fn serve(
    listener: TcpListener,
    feed: Arc<Feed>,
    heartbeat: Duration,
    shutdown: impl Future<Output = ()>,
) {
    let mut conns = tokio::task::JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        let accepted = tokio::select! {
            () = &mut shutdown => return,
            a = listener.accept() => a,
        };
        while conns.try_join_next().is_some() {}
        let Ok((tcp, _)) = accepted else {
            continue;
        };
        let feed = Arc::clone(&feed);
        conns.spawn(async move {
            let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                let feed = Arc::clone(&feed);
                async move { Ok::<_, Infallible>(stream(&feed, &req, heartbeat)) }
            });
            let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(tcp), service)
                .await;
        });
    }
}

/// The answer to one subscription.
fn stream(feed: &Arc<Feed>, req: &Request<Incoming>, heartbeat: Duration) -> Response<Lines> {
    if req.method() != http::Method::GET {
        return Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Lines::empty())
            .expect("a valid response");
    }
    let since = req
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .find_map(|kv| kv.strip_prefix("since="))
        .map(str::to_owned);
    let (tx, rx) = mpsc::channel::<Bytes>(64);
    let feed = Arc::clone(feed);
    tokio::spawn(async move {
        let mut tick = feed.tick.subscribe();
        let mut sent = match feed.start(since.as_deref()) {
            Start::Snapshot(version, eps) => {
                let first = line(&Out::Snapshot {
                    feed: feed.id(),
                    version,
                    endpoints: &eps,
                });
                if tx.send(first).await.is_err() {
                    return;
                }
                version
            }
            Start::Resume(version) => {
                let first = line(&Out::Resume {
                    feed: feed.id(),
                    version,
                });
                if tx.send(first).await.is_err() {
                    return;
                }
                version
            }
        };
        loop {
            tick.borrow_and_update();
            // A stream that fell behind what the feed keeps ends; its
            // client comes back with its token and gets a snapshot.
            let Some(changes) = feed.after(sent) else {
                return;
            };
            for (version, d) in &changes {
                let l = match d {
                    Delta::Upsert(ep) => line(&Out::Upsert {
                        version: *version,
                        endpoint: ep,
                    }),
                    Delta::Remove(a) => line(&Out::Remove {
                        version: *version,
                        address: a,
                    }),
                };
                if tx.send(l).await.is_err() {
                    return;
                }
                sent = *version;
            }
            tokio::select! {
                r = tick.changed() => if r.is_err() { return },
                () = tokio::time::sleep(heartbeat) => {
                    if tx.send(line(&Out::Heartbeat { version: sent })).await.is_err() {
                        return;
                    }
                }
                () = tx.closed() => return,
            }
        }
    });
    Response::builder()
        .header(http::header::CONTENT_TYPE, "application/x-ndjson")
        .body(Lines(Some(rx)))
        .expect("a valid response")
}

/// A response body of lines from a channel.
pub struct Lines(Option<mpsc::Receiver<Bytes>>);

impl Lines {
    fn empty() -> Lines {
        Lines(None)
    }
}

impl Body for Lines {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match &mut self.get_mut().0 {
            Some(rx) => rx.poll_recv(cx).map(|b| b.map(|b| Ok(Frame::data(b)))),
            None => Poll::Ready(None),
        }
    }
}

/// A discovery source following a delta feed.
#[derive(Debug, Clone)]
pub struct DeltaSource {
    /// `http://host:port/path` or `https://host:port/path`.
    pub url: String,
    /// The CA the feed's certificate must chain to; required for https.
    pub ca_file: Option<PathBuf>,
    /// A bearer token file, read for each connection; https only.
    pub token_file: Option<PathBuf>,
    /// The first wait after a break; it doubles to 10 s.
    pub retry: Duration,
    /// How long a stream may stay silent before it is given up.
    pub idle: Duration,
}

impl DeltaSource {
    /// A source following `url`, with a 100 ms first retry and a 15 s
    /// silence limit (three heartbeats).
    pub fn new(url: &str) -> DeltaSource {
        DeltaSource {
            url: url.to_owned(),
            ca_file: None,
            token_file: None,
            retry: Duration::from_millis(100),
            idle: HEARTBEAT * 3,
        }
    }

    async fn follow(self, tx: mpsc::Sender<UpdateKind>) {
        let mut token: Option<(String, u64)> = None;
        let mut backoff = self.retry;
        loop {
            let before = token.clone();
            let ended = tokio::select! {
                e = self.stream(&mut token, &tx) => e,
                () = tx.closed() => return,
            };
            let Err(Break { error, protocol }) = ended else {
                return;
            };
            // A stream that got anywhere starts the backoff again.
            if token.is_some() && token != before {
                backoff = self.retry;
            }
            if protocol {
                token = None;
            }
            if tx.send(UpdateKind::Failed(error)).await.is_err() {
                return;
            }
            tokio::select! {
                () = tokio::time::sleep(backoff) => {}
                () = tx.closed() => return,
            }
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    }

    /// One stream, until it breaks. `Ok` only when the receiver is gone.
    async fn stream(
        &self,
        token: &mut Option<(String, u64)>,
        tx: &mpsc::Sender<UpdateKind>,
    ) -> Result<(), Break> {
        let (tls, rest) = if let Some(r) = self.url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = self.url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(Break::io(format!("{}: not an http(s) URL", self.url)));
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let mut target = path.to_owned();
        if let Some((id, v)) = token.as_ref() {
            target.push(if target.contains('?') { '&' } else { '?' });
            target.push_str(&format!("since={id}.{v}"));
        }
        let mut req = Request::get(format!(
            "{}://{authority}{target}",
            if tls { "https" } else { "http" }
        ))
        .header(http::header::ACCEPT, "application/x-ndjson");
        if let Some(file) = &self.token_file {
            if !tls {
                return Err(Break::io(format!(
                    "{}: a token is only sent over https://",
                    self.url
                )));
            }
            let t = std::fs::read_to_string(file)
                .map_err(|e| Break::io(format!("{}: {e}", file.display())))?;
            req = req.header(http::header::AUTHORIZATION, format!("Bearer {}", t.trim()));
        }
        let req = req
            .body(Empty::<Bytes>::new())
            .map_err(|e| Break::io(e.to_string()))?;
        let tcp = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(authority))
            .await
            .map_err(|_| Break::io(format!("{authority}: connect timed out")))?
            .map_err(|e| Break::io(format!("{authority}: {e}")))?;
        let _ = tcp.set_nodelay(true);
        let resp = if tls {
            let ca = self
                .ca_file
                .as_ref()
                .ok_or_else(|| Break::io(format!("{}: https:// needs a CA file", self.url)))?;
            let t = TlsTo::load(&UpstreamTls {
                ca_file: ca.display().to_string(),
                server_name: None,
                cert_file: None,
                key_file: None,
            })
            .map_err(Break::io)?
            .with_alpn(vec![b"h2".to_vec()]);
            let host = authority
                .rsplit_once(':')
                .map_or(authority, |(h, _)| h)
                .trim_start_matches('[')
                .trim_end_matches(']');
            let name = rustls::pki_types::ServerName::try_from(host.to_owned())
                .map_err(|e| Break::io(e.to_string()))?;
            let stream = tokio_rustls::TlsConnector::from(t.client_config())
                .connect(name, tcp)
                .await
                .map_err(|e| Break::io(format!("{authority}: tls: {e}")))?;
            send(TokioIo::new(stream), req).await?
        } else {
            send(TokioIo::new(tcp), req).await?
        };
        if resp.status() != StatusCode::OK {
            return Err(Break::io(format!("{}: {}", self.url, resp.status())));
        }
        self.read(resp.into_body(), token, tx).await
    }

    async fn read(
        &self,
        mut body: Incoming,
        token: &mut Option<(String, u64)>,
        tx: &mpsc::Sender<UpdateKind>,
    ) -> Result<(), Break> {
        let mut buf = BytesMut::new();
        let mut first = true;
        loop {
            let frame = match tokio::time::timeout(self.idle, body.frame()).await {
                Err(_) => return Err(Break::io(format!("silent for {:?}", self.idle))),
                Ok(None) => return Err(Break::io("the feed ended the stream".into())),
                Ok(Some(Err(e))) => return Err(Break::io(e.to_string())),
                Ok(Some(Ok(f))) => f,
            };
            let Ok(data) = frame.into_data() else {
                continue;
            };
            buf.extend_from_slice(&data);
            while let Some(end) = buf.iter().position(|b| *b == b'\n') {
                let l = buf.split_to(end + 1);
                let update = self.apply(&l[..end], token, first)?;
                first = false;
                if let Some(u) = update
                    && tx.send(u).await.is_err()
                {
                    return Ok(());
                }
            }
            if buf.len() > MAX_LINE_BYTES {
                return Err(Break::protocol(format!(
                    "a line over {MAX_LINE_BYTES} bytes"
                )));
            }
        }
    }

    /// The update one line makes, checking it continues the stream.
    fn apply(
        &self,
        l: &[u8],
        token: &mut Option<(String, u64)>,
        first: bool,
    ) -> Result<Option<UpdateKind>, Break> {
        let msg: In = serde_json::from_slice(l).map_err(|e| Break::protocol(e.to_string()))?;
        let next = |token: &Option<(String, u64)>, version: u64| match token {
            Some((_, v)) if *v + 1 == version => Ok(()),
            Some((_, v)) => Err(Break::protocol(format!(
                "version {version} after {v}: a change was skipped or repeated"
            ))),
            None => Err(Break::protocol("a change before any snapshot".into())),
        };
        match msg {
            In::Snapshot {
                feed,
                version,
                endpoints,
            } => {
                if !first {
                    return Err(Break::protocol("a snapshot mid-stream".into()));
                }
                let eps = endpoints
                    .into_iter()
                    .enumerate()
                    .map(|(i, e)| e.check(&format!("endpoints[{i}]")))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(Break::protocol)?;
                *token = Some((feed, version));
                Ok(Some(UpdateKind::Set(eps)))
            }
            In::Resume { feed, version } => match token {
                Some((id, v)) if first && *id == feed && *v == version => Ok(None),
                _ => Err(Break::protocol(format!(
                    "resumed {feed}.{version}, which is not where this stream was"
                ))),
            },
            In::Upsert { version, endpoint } => {
                next(token, version)?;
                let ep = endpoint.check("endpoint").map_err(Break::protocol)?;
                if let Some((_, v)) = token.as_mut() {
                    *v = version;
                }
                Ok(Some(UpdateKind::Upsert(ep)))
            }
            In::Remove { version, address } => {
                next(token, version)?;
                if let Some((_, v)) = token.as_mut() {
                    *v = version;
                }
                Ok(Some(UpdateKind::Remove(address)))
            }
            In::Heartbeat { version } => match token {
                Some((_, v)) if *v == version => Ok(None),
                _ => Err(Break::protocol(format!(
                    "a heartbeat at {version}, not where this stream was"
                ))),
            },
        }
    }
}

/// Why a stream stopped.
struct Break {
    error: String,
    /// The stream broke the protocol, so its token is not trusted.
    protocol: bool,
}

impl Break {
    fn io(error: String) -> Break {
        Break {
            error,
            protocol: false,
        }
    }

    fn protocol(error: String) -> Break {
        Break {
            error: format!("delta feed: {error}"),
            protocol: true,
        }
    }
}

async fn send<T>(io: TokioIo<T>, req: Request<Empty<Bytes>>) -> Result<Response<Incoming>, Break>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io)
        .await
        .map_err(|e| Break::io(format!("http/2: {e}")))?;
    tokio::spawn(conn);
    tokio::time::timeout(Duration::from_secs(10), sender.send_request(req))
        .await
        .map_err(|_| Break::io("no answer within 10 s".into()))?
        .map_err(|e| Break::io(e.to_string()))
}

impl Source for DeltaSource {
    fn name(&self) -> String {
        format!("delta:{}", self.url)
    }

    fn run(
        self: Box<Self>,
        tx: mpsc::Sender<UpdateKind>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin((*self).follow(tx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(a: &str) -> Endpoint {
        Endpoint::new(a)
    }

    #[test]
    fn versions_rise_by_one_per_change_and_not_for_none() {
        let f = Feed::new(10);
        assert_eq!(f.upsert(ep("a:1")), 1);
        assert_eq!(f.upsert(ep("a:1")), 1);
        assert_eq!(f.remove("b:1"), 1);
        assert_eq!(f.set(vec![ep("a:1"), ep("b:1")]), 2);
        assert_eq!(f.set(vec![ep("c:1")]), 5);
        assert_eq!(f.endpoints(), [ep("c:1")]);
    }

    #[test]
    fn tokens_resume_only_within_the_kept_changes_of_the_same_feed() {
        let f = Feed::new(3);
        for i in 0..5 {
            f.upsert(ep(&format!("h:{}", i + 1)));
        }
        let id = f.id().to_owned();
        let token = |v: u64| format!("{id}.{v}");
        assert!(matches!(f.start(Some(&token(5))), Start::Resume(5)));
        assert!(matches!(f.start(Some(&token(2))), Start::Resume(2)));
        assert!(matches!(f.start(Some(&token(1))), Start::Snapshot(5, _)));
        assert!(matches!(f.start(Some(&token(6))), Start::Snapshot(..)));
        assert!(matches!(f.start(Some("other.5")), Start::Snapshot(..)));
        assert!(matches!(f.start(Some("junk")), Start::Snapshot(..)));
        assert!(matches!(f.start(None), Start::Snapshot(..)));
        assert_eq!(f.starts(), (5, 2));
        assert_eq!(f.after(2).unwrap().len(), 3);
        assert!(f.after(1).is_none());
        assert!(f.after(5).unwrap().is_empty());
    }

    #[test]
    fn the_client_refuses_gaps_repeats_and_strays() {
        let s = DeltaSource::new("http://feed:1/");
        let mut token = None;
        let snap = br#"{"type":"snapshot","feed":"f","version":3,"endpoints":[{"address":"a:1"}]}"#;
        assert!(matches!(
            s.apply(snap, &mut token, true),
            Ok(Some(UpdateKind::Set(_)))
        ));
        assert_eq!(token, Some(("f".into(), 3)));
        let up =
            |v: u64| format!(r#"{{"type":"upsert","version":{v},"endpoint":{{"address":"b:1"}}}}"#);
        assert!(s.apply(up(4).as_bytes(), &mut token, false).is_ok());
        assert!(
            s.apply(up(4).as_bytes(), &mut token, false)
                .unwrap_err()
                .protocol
        );
        assert!(s.apply(up(6).as_bytes(), &mut token, false).is_err());
        assert!(
            s.apply(br#"{"type":"heartbeat","version":4}"#, &mut token, false)
                .is_ok()
        );
        assert!(
            s.apply(br#"{"type":"heartbeat","version":9}"#, &mut token, false)
                .is_err()
        );
        assert!(s.apply(snap, &mut token, false).is_err());
        assert!(
            s.apply(
                br#"{"type":"resume","feed":"f","version":4}"#,
                &mut token,
                true
            )
            .is_ok()
        );
        assert!(
            s.apply(
                br#"{"type":"resume","feed":"g","version":4}"#,
                &mut token,
                true
            )
            .is_err()
        );
        let bad = br#"{"type":"upsert","version":5,"endpoint":{"address":"b:1","weight":0}}"#;
        assert!(s.apply(bad, &mut token, false).unwrap_err().protocol);
        assert!(s.apply(b"{\"type\":\"wat\"}", &mut token, false).is_err());
        let mut none = None;
        assert!(s.apply(up(1).as_bytes(), &mut none, true).is_err());
    }

    #[test]
    fn names_say_what_is_followed() {
        assert_eq!(
            DeltaSource::new("http://f:1/x").name(),
            "delta:http://f:1/x"
        );
    }
}
