//! Purge propagation between nodes.
//!
//! A purge accepted on one node becomes a signed message sent to every
//! peer. Each node that accepts a new message applies it and relays it
//! once to its own peers, so a purge still spreads when the first node
//! cannot reach everyone. Message ids stop loops and replays.
//!
//! Messages are signed with HMAC-SHA256 (RFC 2104) over the exact bytes of
//! the body, and the signature is checked before the body is parsed. The
//! signed body carries the issuing time; messages older than the retry
//! window, or from too far in the future, are refused, and ids are
//! remembered for that window, so a captured message cannot be replayed.
//! The key is shared by the cluster and never sent.

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use http::header::{self, HeaderName, HeaderValue};
use http::uri::Authority;
use http::{Method, Request, Response, StatusCode, Uri};
use hyper::body::Incoming;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::admin::{Refusal, read_body};
use crate::ids::new_id;
use crate::object::{MAX_TAG_LEN, valid_tag};
use crate::origin::Client;
use crate::proxy::Proxy;
use crate::purge::{MAX_PURGE_BODY, MAX_TARGETS, PurgeTarget};
use crate::respond::{Body, full};

pub const SIGNATURE: HeaderName = HeaderName::from_static("x-cachet-signature");
pub const SENDER: HeaderName = HeaderName::from_static("x-cachet-node");
pub const PEER_PATH: &str = "/peer/purge";
const MAX_KEY_LEN: usize = 8 * 1024;

/// A purge as it travels between nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PurgeMessage {
    pub id: String,
    /// The node that accepted the purge.
    pub origin: String,
    /// Unix seconds when the origin node accepted it.
    pub issued_at: u64,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub keys: Vec<String>,
}

impl PurgeMessage {
    fn targets(&self) -> Result<Vec<PurgeTarget>, &'static str> {
        if self.tags.len() + self.keys.len() > MAX_TARGETS {
            return Err("too many targets");
        }
        let mut out = Vec::with_capacity(self.tags.len() + self.keys.len());
        for tag in &self.tags {
            if !valid_tag(tag) || tag.len() > MAX_TAG_LEN {
                return Err("invalid tag");
            }
            out.push(PurgeTarget::Tag(tag.as_str().into()));
        }
        for key in &self.keys {
            if key.is_empty() || key.len() > MAX_KEY_LEN {
                return Err("invalid key");
            }
            out.push(PurgeTarget::Key(key.as_str().into()));
        }
        Ok(out)
    }

    fn from_targets(origin: &str, targets: &[PurgeTarget], issued_at: u64) -> Self {
        let mut msg = PurgeMessage {
            id: new_id(),
            origin: origin.to_string(),
            issued_at,
            tags: Vec::new(),
            keys: Vec::new(),
        };
        for t in targets {
            match t {
                PurgeTarget::Tag(tag) => msg.tags.push(tag.to_string()),
                PurgeTarget::Key(key) => msg.keys.push(key.to_string()),
            }
        }
        msg
    }
}

/// `v1=` followed by the lowercase hex HMAC-SHA256 of `body` under `key`.
pub fn sign(key: &[u8], body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(body);
    let tag = mac.finalize().into_bytes();
    let mut out = String::with_capacity(3 + tag.len() * 2);
    out.push_str("v1=");
    for b in tag {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn unhex(s: &[u8]) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    s.chunks(2)
        .map(|p| Some(digit(p[0])? << 4 | digit(p[1])?))
        .collect()
}

/// Checks a signature header against `body`, in constant time.
pub fn verify(key: &[u8], body: &[u8], signature: &[u8]) -> bool {
    let Some(tag) = signature.strip_prefix(b"v1=").and_then(unhex) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(body);
    mac.verify_slice(&tag).is_ok()
}

#[derive(Debug, Clone)]
pub struct GossipSettings {
    /// How long a message is retried, accepted and remembered.
    pub max_age: Duration,
    /// How far in the future a message's time may be.
    pub max_skew: Duration,
    pub retry_initial: Duration,
    pub retry_max: Duration,
    pub request_timeout: Duration,
    /// Messages waiting per peer before new ones are dropped.
    pub queue: usize,
}

impl Default for GossipSettings {
    fn default() -> Self {
        GossipSettings {
            max_age: Duration::from_secs(600),
            max_skew: Duration::from_secs(60),
            retry_initial: Duration::from_millis(100),
            retry_max: Duration::from_secs(5),
            request_timeout: Duration::from_secs(2),
            queue: 1024,
        }
    }
}

/// A message ready to send: the exact signed bytes and their signature.
struct Envelope {
    body: Bytes,
    signature: HeaderValue,
    issued_at: u64,
}

struct Peer {
    name: String,
    queue: mpsc::Sender<Arc<Envelope>>,
}

#[derive(Default)]
struct Seen {
    ids: HashSet<String>,
    order: VecDeque<(u64, String)>,
}

pub struct Cluster {
    node_id: String,
    key: Vec<u8>,
    peers: Vec<Peer>,
    seen: Mutex<Seen>,
    proxy: Arc<Proxy>,
    settings: GossipSettings,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn json(status: StatusCode, value: &serde_json::Value) -> Response<Body> {
    let mut resp = Response::new(full(Bytes::from(value.to_string())));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

fn refuse(status: StatusCode, message: &'static str) -> Response<Body> {
    Refusal { status, message }.into_response()
}

impl Cluster {
    /// Builds the cluster and starts one delivery task per peer.
    pub fn start(
        node_id: &str,
        key: Vec<u8>,
        peers: &[(String, Authority)],
        proxy: Arc<Proxy>,
        client: Client,
        settings: GossipSettings,
    ) -> (Arc<Cluster>, Vec<JoinHandle<()>>) {
        let mut tasks = Vec::new();
        let mut list = Vec::new();
        for (name, authority) in peers {
            let (tx, rx) = mpsc::channel(settings.queue.max(1));
            list.push(Peer {
                name: name.clone(),
                queue: tx,
            });
            let delivery = Delivery {
                peer: name.clone(),
                uri: Uri::builder()
                    .scheme("http")
                    .authority(authority.clone())
                    .path_and_query(PEER_PATH)
                    .build()
                    .expect("valid peer URI"),
                node_id: HeaderValue::from_str(node_id).expect("node id is a token"),
                client: client.clone(),
                proxy: proxy.clone(),
                settings: settings.clone(),
            };
            tasks.push(tokio::spawn(delivery.run(rx)));
        }
        let cluster = Arc::new(Cluster {
            node_id: node_id.to_string(),
            key,
            peers: list,
            seen: Mutex::new(Seen::default()),
            proxy,
            settings,
        });
        (cluster, tasks)
    }

    fn seen(&self) -> MutexGuard<'_, Seen> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records a message id; false if it was already known. Ids are kept
    /// for the acceptance window, after which age checks refuse them.
    fn remember(&self, id: &str, issued_at: u64) -> bool {
        let now = unix_now();
        let mut seen = self.seen();
        while let Some((expiry, _)) = seen.order.front() {
            if *expiry >= now {
                break;
            }
            let (_, old) = seen.order.pop_front().expect("front exists");
            seen.ids.remove(&old);
        }
        if seen.ids.contains(id) {
            return false;
        }
        let expiry = issued_at + self.settings.max_age.as_secs() + self.settings.max_skew.as_secs();
        seen.ids.insert(id.to_string());
        seen.order.push_back((expiry, id.to_string()));
        true
    }

    fn envelope(&self, msg: &PurgeMessage) -> Arc<Envelope> {
        let body = Bytes::from(serde_json::to_vec(msg).expect("message serialises"));
        let signature =
            HeaderValue::from_str(&sign(&self.key, &body)).expect("hex is a valid header value");
        Arc::new(Envelope {
            body,
            signature,
            issued_at: msg.issued_at,
        })
    }

    fn send_to_peers(&self, envelope: &Arc<Envelope>, skip: &[&str]) {
        for peer in self
            .peers
            .iter()
            .filter(|p| !skip.contains(&p.name.as_str()))
        {
            if peer.queue.try_send(envelope.clone()).is_err() {
                self.proxy
                    .metrics
                    .gossip_dropped
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(peer = %peer.name, "purge queue full; message dropped for this peer");
            }
        }
    }

    /// Applies a purge accepted through the admin API and sends it on.
    pub async fn purge_local(&self, targets: &[PurgeTarget]) -> (usize, String) {
        let msg = PurgeMessage::from_targets(&self.node_id, targets, unix_now());
        self.remember(&msg.id, msg.issued_at);
        let removed = self.proxy.cache.purge(targets).await;
        self.send_to_peers(&self.envelope(&msg), &[]);
        (removed, msg.id)
    }

    /// Handles `POST /peer/purge`.
    pub async fn receive(&self, req: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
        if req.method() != Method::POST {
            return refuse(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        let signature = req.headers().get(&SIGNATURE).cloned();
        let sender = req
            .headers()
            .get(&SENDER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = match read_body(req.into_body(), MAX_PURGE_BODY * 2).await {
            Ok(b) => b,
            Err(refusal) => return refusal.into_response(),
        };
        let reject = |status, message| {
            self.proxy
                .metrics
                .gossip_rejected
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!(peer = %peer, sender = %sender, reason = message, "peer purge rejected");
            refuse(status, message)
        };
        let Some(signature) = signature.filter(|s| verify(&self.key, &body, s.as_bytes())) else {
            return reject(StatusCode::UNAUTHORIZED, "bad signature");
        };
        let Ok(msg) = serde_json::from_slice::<PurgeMessage>(&body) else {
            return reject(StatusCode::BAD_REQUEST, "malformed message");
        };
        let now = unix_now();
        if msg.issued_at + self.settings.max_age.as_secs() < now {
            return reject(StatusCode::BAD_REQUEST, "message expired");
        }
        if msg.issued_at > now + self.settings.max_skew.as_secs() {
            return reject(StatusCode::BAD_REQUEST, "message from the future");
        }
        let targets = match msg.targets() {
            Ok(t) => t,
            Err(e) => return reject(StatusCode::BAD_REQUEST, e),
        };
        if msg.origin == self.node_id || !self.remember(&msg.id, msg.issued_at) {
            return json(StatusCode::OK, &serde_json::json!({ "duplicate": true }));
        }
        let removed = self.proxy.cache.purge(&targets).await;
        let m = &self.proxy.metrics;
        m.purges_peer.fetch_add(1, Ordering::Relaxed);
        m.purged_objects
            .fetch_add(removed as u64, Ordering::Relaxed);
        tracing::info!(id = %msg.id, origin = %msg.origin, sender = %sender, removed, "peer purge applied");
        // Relay the same signed bytes; the signature stays valid.
        let envelope = Arc::new(Envelope {
            signature,
            body,
            issued_at: msg.issued_at,
        });
        self.send_to_peers(&envelope, &[&msg.origin, &sender]);
        json(StatusCode::OK, &serde_json::json!({ "removed": removed }))
    }
}

/// Delivers messages to one peer, in order, retrying with backoff until
/// the message would be too old for the peer to accept.
struct Delivery {
    peer: String,
    uri: Uri,
    node_id: HeaderValue,
    client: Client,
    proxy: Arc<Proxy>,
    settings: GossipSettings,
}

enum Attempt {
    Delivered,
    Refused(StatusCode),
    Failed,
}

impl Delivery {
    async fn run(self, mut rx: mpsc::Receiver<Arc<Envelope>>) {
        while let Some(envelope) = rx.recv().await {
            self.deliver(&envelope).await;
        }
    }

    async fn attempt(&self, envelope: &Envelope) -> Attempt {
        let mut req = Request::new(full(envelope.body.clone()));
        *req.method_mut() = Method::POST;
        *req.uri_mut() = self.uri.clone();
        let headers = req.headers_mut();
        headers.insert(SIGNATURE, envelope.signature.clone());
        headers.insert(SENDER, self.node_id.clone());
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        match tokio::time::timeout(self.settings.request_timeout, self.client.request(req)).await {
            Ok(Ok(resp)) if resp.status().is_success() => Attempt::Delivered,
            // A peer that refuses the message (bad key, malformed) will not
            // change its mind; overload and server errors are retried.
            Ok(Ok(resp))
                if resp.status().is_client_error()
                    && resp.status() != StatusCode::TOO_MANY_REQUESTS =>
            {
                Attempt::Refused(resp.status())
            }
            _ => Attempt::Failed,
        }
    }

    async fn deliver(&self, envelope: &Envelope) {
        let m = &self.proxy.metrics;
        let mut backoff = self.settings.retry_initial;
        loop {
            match self.attempt(envelope).await {
                Attempt::Delivered => {
                    m.gossip_sent.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Attempt::Refused(status) => {
                    m.gossip_dropped.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(peer = %self.peer, status = status.as_u16(), "peer refused purge message");
                    return;
                }
                Attempt::Failed => {}
            }
            if envelope.issued_at + self.settings.max_age.as_secs() <= unix_now() {
                m.gossip_dropped.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(peer = %self.peer, "giving up on purge message: retry window passed");
                return;
            }
            m.gossip_retries.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(self.settings.retry_max);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        let key = b"k".repeat(32);
        let sig = sign(&key, b"body");
        assert!(sig.starts_with("v1=") && sig.len() == 3 + 64);
        assert!(verify(&key, b"body", sig.as_bytes()));
        assert!(!verify(&key, b"body!", sig.as_bytes()));
        assert!(!verify(&b"x".repeat(32), b"body", sig.as_bytes()));
        assert!(!verify(&key, b"body", b"v2=00"));
        assert!(!verify(&key, b"body", b"v1=zz"));
        assert!(!verify(&key, b"body", b"v1=abc"));
        assert!(!verify(&key, b"body", &sig.as_bytes()[..10]));
        // RFC 4231 test case 2.
        let tc2 = sign(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            tc2,
            "v1=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn message_targets() {
        let msg = PurgeMessage::from_targets(
            "a",
            &[PurgeTarget::Tag("t".into()), PurgeTarget::Key("h/x".into())],
            1,
        );
        assert_eq!(msg.tags, ["t"]);
        assert_eq!(msg.keys, ["h/x"]);
        assert_eq!(msg.targets().unwrap().len(), 2);
        let mut bad = msg.clone();
        bad.tags = vec!["has space".into()];
        assert_eq!(bad.targets().unwrap_err(), "invalid tag");
        let mut bad = msg.clone();
        bad.keys = vec![String::new()];
        assert_eq!(bad.targets().unwrap_err(), "invalid key");
        let mut bad = msg;
        bad.tags = (0..=MAX_TARGETS).map(|i| format!("t{i}")).collect();
        assert_eq!(bad.targets().unwrap_err(), "too many targets");
    }
}
