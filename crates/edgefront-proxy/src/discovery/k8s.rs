//! Endpoints from a Kubernetes Service's EndpointSlices, listed and then
//! watched over the API server's HTTP API.
//!
//! The client is written here, over the HTTP/1.1 client edgefront already
//! has, rather than taken from kube-rs: two requests (a list and a watch of
//! one resource) do not earn kube-rs's size, which measured 5,878,064
//! bytes on the release binary (14,090,192 to 19,968,256 on an arm64 Mac),
//! against the roadmap's 20 MB image.
//!
//! - **What is watched.** The `discovery.k8s.io/v1` EndpointSlices in one
//!   namespace labelled `kubernetes.io/service-name=<service>`: listed in
//!   pages of 500, then watched from the list's resource version with
//!   bookmarks on. A watch that ends (the server closes every watch after
//!   `timeoutSeconds`, 290 here) resumes from the last version seen; a
//!   version the server has compacted away (410 Gone) lists again. That is
//!   routine and reported to nobody.
//! - **Addresses.** Each IPv4 or IPv6 slice gives one endpoint per address,
//!   on the slice's port of the configured name, or its first port when no
//!   name is configured. FQDN slices, and slices without that port, add
//!   nothing.
//! - **Readiness.** An endpoint is ready unless its `ready` condition is
//!   false: Kubernetes leaves it unset when it does not know, and says to
//!   treat that as ready. A pod failing its readiness probe therefore stays
//!   in the set with `ready: false`, so the flip is a change, not a removal.
//! - **Zone.** The endpoint's `zone` becomes its zone.
//! - **Several slices.** An address two slices name (while the control
//!   plane moves endpoints between them) is kept once, ready if either says
//!   so.
//! - **Updates.** After each full list the source sends the whole set, and
//!   again on every slice change, which the merger turns into a delta. A
//!   failed list or watch, or an error event, is sent as a failure, so the
//!   merger keeps the last good set, and is retried after 500 ms, doubling
//!   to 30 s.
//! - **Credentials.** In a pod, the API server comes from
//!   `KUBERNETES_SERVICE_HOST` and `KUBERNETES_SERVICE_PORT`, with the
//!   service account's token and CA from
//!   `/var/run/secrets/kubernetes.io/serviceaccount`. Outside one,
//!   [`ApiAccess`] names the server, its CA, and a token file or a client
//!   certificate. The token is read again for every request, so rotated
//!   tokens are picked up, and it is only ever sent over TLS to the server
//!   named. Only `list` and `watch` on `endpointslices` in the namespace
//!   are needed.
//! - **Limits.** A list page or a watch event over 16 MiB fails the read.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use bytes::{Buf, Bytes, BytesMut};
use edgefront_core::config::UpstreamTls;
use edgefront_core::http1::{Client, ClientLimits, TlsTo};
use http::{Request, StatusCode};
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use serde::Deserialize;
use tokio::sync::mpsc;

use super::{Endpoint, Source, UpdateKind};

/// The label EndpointSlices carry with their Service's name.
pub const SERVICE_LABEL: &str = "kubernetes.io/service-name";

/// Where a pod's service account credentials are mounted.
pub const SERVICE_ACCOUNT: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

/// The largest list page or watch event read.
pub const MAX_OBJECT_BYTES: usize = 16 << 20;

/// How the API server is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiAccess {
    /// `https://host:port`, or `http://host:port` for a server without TLS
    /// (a local proxy such as `kubectl proxy`, or a test); no credential
    /// is ever sent over `http://`.
    pub server: String,
    /// The CA bundle the server's certificate must chain to.
    pub ca_file: Option<PathBuf>,
    /// A file holding a bearer token, read for every request.
    pub token_file: Option<PathBuf>,
    /// A client certificate and key, instead of or beside a token.
    pub cert_file: Option<PathBuf>,
    pub key_file: Option<PathBuf>,
}

impl ApiAccess {
    /// The server at `server` with no credentials.
    pub fn new(server: &str) -> ApiAccess {
        ApiAccess {
            server: server.trim_end_matches('/').to_owned(),
            ca_file: None,
            token_file: None,
            cert_file: None,
            key_file: None,
        }
    }

    /// The pod's own API server and service account, from the environment.
    pub fn in_cluster() -> Result<ApiAccess, String> {
        let host = std::env::var("KUBERNETES_SERVICE_HOST").map_err(|_| {
            "not in a Kubernetes pod: KUBERNETES_SERVICE_HOST is not set".to_owned()
        })?;
        let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".into());
        let host = match host.parse::<IpAddr>() {
            Ok(IpAddr::V6(v6)) => format!("[{v6}]"),
            _ => host,
        };
        let dir = PathBuf::from(SERVICE_ACCOUNT);
        Ok(ApiAccess {
            server: format!("https://{host}:{port}"),
            ca_file: Some(dir.join("ca.crt")),
            token_file: Some(dir.join("token")),
            cert_file: None,
            key_file: None,
        })
    }

    fn tls(&self) -> bool {
        self.server.starts_with("https://")
    }

    /// The server's `host:port`.
    fn authority(&self) -> Result<&str, String> {
        let rest = self
            .server
            .strip_prefix("https://")
            .or_else(|| self.server.strip_prefix("http://"))
            .ok_or_else(|| format!("{}: not an http:// or https:// URL", self.server))?;
        if rest.is_empty() || rest.contains(['/', '@', '?', '#']) {
            return Err(format!("{}: must be scheme://host:port", self.server));
        }
        Ok(rest)
    }

    fn client(&self) -> Result<Client<Empty<Bytes>>, String> {
        let limits = ClientLimits {
            connect_timeout: Duration::from_secs(5),
            response_header_timeout: Some(Duration::from_secs(30)),
            ..ClientLimits::default()
        };
        if !self.tls() {
            if self.token_file.is_some() || self.cert_file.is_some() {
                return Err(format!(
                    "{}: credentials are only sent over https://",
                    self.server
                ));
            }
            return Ok(Client::new(&limits));
        }
        let ca = self
            .ca_file
            .as_ref()
            .ok_or_else(|| format!("{}: https:// needs a CA file", self.server))?;
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string());
        let tls = TlsTo::load(&UpstreamTls {
            ca_file: ca.display().to_string(),
            server_name: None,
            cert_file: path(&self.cert_file),
            key_file: path(&self.key_file),
        })?;
        Ok(Client::with_tls(&limits, tls))
    }
}

/// A discovery source watching one Service's EndpointSlices.
#[derive(Debug, Clone)]
pub struct KubernetesSource {
    pub namespace: String,
    pub service: String,
    /// The port's name in the slices; the first port when `None`.
    pub port: Option<String>,
    /// The API server; the pod's own when `None`.
    pub api: Option<ApiAccess>,
    /// The first wait after a failure; it doubles to 30 s.
    pub retry: Duration,
}

impl KubernetesSource {
    /// A source for `service` in `namespace` on `port` (the first port
    /// when `None`), through the pod's own API server.
    pub fn new(namespace: &str, service: &str, port: Option<&str>) -> KubernetesSource {
        KubernetesSource {
            namespace: namespace.to_owned(),
            service: service.to_owned(),
            port: port.map(str::to_owned),
            api: None,
            retry: Duration::from_millis(500),
        }
    }

    /// Reaches the API server through `api` instead.
    pub fn api(mut self, api: ApiAccess) -> KubernetesSource {
        self.api = Some(api);
        self
    }

    async fn watch(self, tx: mpsc::Sender<UpdateKind>) {
        let mut backoff = self.retry;
        loop {
            let failure = tokio::select! {
                e = self.session(&tx) => e,
                () = tx.closed() => return,
            };
            match failure {
                Some(e) => {
                    if tx.send(UpdateKind::Failed(e)).await.is_err() {
                        return;
                    }
                    tokio::select! {
                        () = tokio::time::sleep(backoff) => {}
                        () = tx.closed() => return,
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
                None => backoff = self.retry,
            }
        }
    }

    /// One list and the watches that follow it, until a failure (returned)
    /// or a 410 Gone (`None`: list again at once).
    async fn session(&self, tx: &mpsc::Sender<UpdateKind>) -> Option<String> {
        let api = match &self.api {
            Some(a) => a.clone(),
            None => match ApiAccess::in_cluster() {
                Ok(a) => a,
                Err(e) => return Some(format!("kubernetes: {e}")),
            },
        };
        let client = match api.client() {
            Ok(c) => c,
            Err(e) => return Some(format!("kubernetes: {e}")),
        };
        let (mut slices, mut version) = match self.list(&api, &client).await {
            Ok(listed) => listed,
            Err(e) => return Some(format!("kubernetes list: {e}")),
        };
        if tx.send(UpdateKind::Set(merge(&slices))).await.is_err() {
            return None;
        }
        loop {
            match self
                .watch_once(&api, &client, &mut slices, &mut version, tx)
                .await
            {
                Ok(Watched::Ended) => {}
                Ok(Watched::Gone) => return None,
                Err(e) => return Some(format!("kubernetes watch: {e}")),
            }
        }
    }

    fn path(&self) -> String {
        format!(
            "/apis/discovery.k8s.io/v1/namespaces/{}/endpointslices?labelSelector={}%3D{}",
            self.namespace,
            SERVICE_LABEL.replace('/', "%2F"),
            self.service
        )
    }

    async fn get(
        &self,
        api: &ApiAccess,
        client: &Client<Empty<Bytes>>,
        query: &str,
    ) -> Result<hyper::Response<Incoming>, String> {
        let uri = format!("http://{}{}{query}", api.authority()?, self.path());
        let mut req = Request::get(uri).header(http::header::ACCEPT, "application/json");
        if let Some(file) = &api.token_file {
            let token =
                std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
            req = req.header(
                http::header::AUTHORIZATION,
                format!("Bearer {}", token.trim()),
            );
        }
        let req = req.body(Empty::new()).map_err(|e| e.to_string())?;
        let resp = client.send(req).await.map_err(|e| e.to_string())?;
        if resp.status() != StatusCode::OK {
            let status = resp.status();
            let body = read_capped(resp.into_body(), 4096)
                .await
                .unwrap_or_default();
            let message = serde_json::from_slice::<Status>(&body)
                .map(|s| s.message)
                .unwrap_or_default();
            return Err(format!("{status}: {message}"));
        }
        Ok(resp)
    }

    /// Every slice of the Service, page by page, and the version to watch
    /// from.
    async fn list(
        &self,
        api: &ApiAccess,
        client: &Client<Empty<Bytes>>,
    ) -> Result<(BTreeMap<String, Vec<Endpoint>>, String), String> {
        let mut slices = BTreeMap::new();
        let mut cont = String::new();
        loop {
            let mut query = "&limit=500".to_owned();
            if !cont.is_empty() {
                query.push_str("&continue=");
                query.push_str(&cont);
            }
            let resp = self.get(api, client, &query).await?;
            let body = read_capped(resp.into_body(), MAX_OBJECT_BYTES).await?;
            let page: SliceList = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
            for s in page.items {
                slices.insert(s.metadata.name.clone(), self.endpoints(&s));
            }
            match page.metadata.cont {
                Some(c) if !c.is_empty() => cont = c,
                _ => return Ok((slices, page.metadata.resource_version)),
            }
        }
    }

    async fn watch_once(
        &self,
        api: &ApiAccess,
        client: &Client<Empty<Bytes>>,
        slices: &mut BTreeMap<String, Vec<Endpoint>>,
        version: &mut String,
        tx: &mpsc::Sender<UpdateKind>,
    ) -> Result<Watched, String> {
        let query = format!(
            "&watch=1&allowWatchBookmarks=true&timeoutSeconds=290&resourceVersion={version}"
        );
        let resp = self.get(api, client, &query).await?;
        let mut body = resp.into_body();
        let mut buf = BytesMut::new();
        loop {
            // The server closes a watch within timeoutSeconds and sends a
            // bookmark about once a minute, so a long silence is a dead
            // connection.
            let frame = match tokio::time::timeout(Duration::from_secs(330), body.frame()).await {
                Err(_) => return Err("no event or bookmark for 330 s".into()),
                Ok(None) => return Ok(Watched::Ended),
                Ok(Some(Err(e))) => return Err(e.to_string()),
                Ok(Some(Ok(f))) => f,
            };
            let Ok(data) = frame.into_data() else {
                continue;
            };
            buf.extend_from_slice(&data);
            while let Some(end) = buf.iter().position(|b| *b == b'\n') {
                let line = buf.split_to(end + 1);
                let line = &line[..line.len() - 1];
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let event: WatchEvent =
                    serde_json::from_slice(line).map_err(|e| format!("event: {e}"))?;
                match event.kind.as_str() {
                    "ERROR" => {
                        let status: Status =
                            serde_json::from_value(event.object).map_err(|e| e.to_string())?;
                        if status.code == 410 {
                            return Ok(Watched::Gone);
                        }
                        return Err(format!("{} {}", status.code, status.message));
                    }
                    "BOOKMARK" => {
                        let s: SliceMeta =
                            serde_json::from_value(event.object).map_err(|e| e.to_string())?;
                        *version = s.metadata.resource_version;
                    }
                    kind @ ("ADDED" | "MODIFIED" | "DELETED") => {
                        let s: Slice =
                            serde_json::from_value(event.object).map_err(|e| e.to_string())?;
                        version.clone_from(&s.metadata.resource_version);
                        if kind == "DELETED" {
                            slices.remove(&s.metadata.name);
                        } else {
                            slices.insert(s.metadata.name.clone(), self.endpoints(&s));
                        }
                        if tx.send(UpdateKind::Set(merge(slices))).await.is_err() {
                            return Ok(Watched::Ended);
                        }
                    }
                    other => return Err(format!("unknown event type {other:?}")),
                }
            }
            if buf.len() > MAX_OBJECT_BYTES {
                return Err(format!("an event over {MAX_OBJECT_BYTES} bytes"));
            }
        }
    }

    /// The endpoints one slice gives (see the module documentation).
    pub fn endpoints(&self, slice: &Slice) -> Vec<Endpoint> {
        if slice.address_type != "IPv4" && slice.address_type != "IPv6" {
            return Vec::new();
        }
        let port = match &self.port {
            Some(name) => slice.ports.iter().find(|p| p.name.as_deref() == Some(name)),
            None => slice.ports.first(),
        };
        let Some(port) = port
            .and_then(|p| p.port)
            .and_then(|p| u16::try_from(p).ok())
        else {
            return Vec::new();
        };
        if port == 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        for ep in &slice.endpoints {
            let ready = ep.conditions.ready.unwrap_or(true);
            for a in &ep.addresses {
                let Ok(ip) = a.parse::<IpAddr>() else {
                    continue;
                };
                let mut e = Endpoint::new(SocketAddr::new(ip, port).to_string());
                e.ready = ready;
                e.zone.clone_from(&ep.zone);
                out.push(e);
            }
        }
        out
    }
}

enum Watched {
    /// The server ended the watch; watch again from the last version.
    Ended,
    /// The version is gone; list again.
    Gone,
}

/// Reads `body` whole, failing past `cap` bytes.
async fn read_capped(mut body: Incoming, cap: usize) -> Result<Vec<u8>, String> {
    let mut out = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| e.to_string())?;
        if let Ok(mut data) = frame.into_data() {
            if out.len() + data.remaining() > cap {
                return Err(format!("a response over {cap} bytes"));
            }
            out.extend_from_slice(&data.copy_to_bytes(data.remaining()));
        }
    }
    Ok(out.to_vec())
}

/// Every slice's endpoints as one set, in slice-name order: an address
/// two slices name is kept once, ready if either says so.
pub fn merge(slices: &BTreeMap<String, Vec<Endpoint>>) -> Vec<Endpoint> {
    let mut out: Vec<Endpoint> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for ep in slices.values().flatten() {
        match at.get(&ep.address) {
            Some(&i) => out[i].ready |= ep.ready,
            None => {
                at.insert(ep.address.clone(), out.len());
                out.push(ep.clone());
            }
        }
    }
    out
}

impl Source for KubernetesSource {
    fn name(&self) -> String {
        match &self.port {
            Some(p) => format!("kubernetes:{}/{}:{p}", self.namespace, self.service),
            None => format!("kubernetes:{}/{}", self.namespace, self.service),
        }
    }

    fn run(
        self: Box<Self>,
        tx: mpsc::Sender<UpdateKind>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin((*self).watch(tx))
    }
}

/// The parts of an EndpointSlice read here; the rest is ignored.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Slice {
    pub metadata: Meta,
    #[serde(default)]
    pub address_type: String,
    #[serde(default)]
    pub ports: Vec<SlicePort>,
    #[serde(default)]
    pub endpoints: Vec<SliceEndpoint>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub resource_version: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SlicePort {
    pub name: Option<String>,
    pub port: Option<i32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SliceEndpoint {
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(default)]
    pub conditions: Conditions,
    pub zone: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Conditions {
    pub ready: Option<bool>,
}

#[derive(Deserialize)]
struct SliceList {
    metadata: ListMeta,
    #[serde(default)]
    items: Vec<Slice>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListMeta {
    #[serde(default)]
    resource_version: String,
    #[serde(rename = "continue")]
    cont: Option<String>,
}

#[derive(Deserialize)]
struct SliceMeta {
    metadata: Meta,
}

#[derive(Deserialize)]
struct WatchEvent {
    #[serde(rename = "type")]
    kind: String,
    object: serde_json::Value,
}

#[derive(Deserialize, Default)]
struct Status {
    #[serde(default)]
    code: u16,
    #[serde(default)]
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(kind: &str, ports: &[(Option<&str>, i32)], eps: &[(&str, Option<bool>)]) -> Slice {
        Slice {
            address_type: kind.into(),
            ports: ports
                .iter()
                .map(|(n, p)| SlicePort {
                    name: n.map(str::to_owned),
                    port: Some(*p),
                })
                .collect(),
            endpoints: eps
                .iter()
                .map(|(a, ready)| SliceEndpoint {
                    addresses: vec![(*a).to_owned()],
                    conditions: Conditions { ready: *ready },
                    zone: Some("zone-a".into()),
                })
                .collect(),
            ..Slice::default()
        }
    }

    fn src(port: Option<&str>) -> KubernetesSource {
        KubernetesSource::new("prod", "api", port)
    }

    #[test]
    fn addresses_take_the_named_port_readiness_and_zone() {
        let s = slice(
            "IPv4",
            &[(Some("metrics"), 9090), (Some("http"), 8080)],
            &[
                ("10.1.0.1", Some(true)),
                ("10.1.0.2", Some(false)),
                ("10.1.0.3", None),
            ],
        );
        let eps = src(Some("http")).endpoints(&s);
        let got: Vec<(&str, bool)> = eps.iter().map(|e| (&*e.address, e.ready)).collect();
        assert_eq!(
            got,
            [
                ("10.1.0.1:8080", true),
                ("10.1.0.2:8080", false),
                ("10.1.0.3:8080", true)
            ]
        );
        assert!(eps.iter().all(|e| e.zone.as_deref() == Some("zone-a")));
        assert_eq!(src(None).endpoints(&s)[0].address, "10.1.0.1:9090");
    }

    #[test]
    fn slices_without_the_port_or_of_fqdns_add_nothing() {
        let s = slice("IPv4", &[(Some("http"), 8080)], &[("10.1.0.1", None)]);
        assert!(src(Some("grpc")).endpoints(&s).is_empty());
        let f = slice("FQDN", &[(None, 80)], &[("api.example", None)]);
        assert!(src(None).endpoints(&f).is_empty());
        let zero = slice("IPv4", &[(None, 0)], &[("10.1.0.1", None)]);
        assert!(src(None).endpoints(&zero).is_empty());
        let none = slice("IPv4", &[], &[("10.1.0.1", None)]);
        assert!(src(None).endpoints(&none).is_empty());
    }

    #[test]
    fn ipv6_addresses_are_bracketed() {
        let s = slice(
            "IPv6",
            &[(None, 443)],
            &[("fd00::5", None), ("not-an-ip", None)],
        );
        let eps = src(None).endpoints(&s);
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].address, "[fd00::5]:443");
    }

    #[test]
    fn an_address_in_two_slices_is_kept_once_and_ready_if_either_is() {
        let mut slices = BTreeMap::new();
        let mut a = Endpoint::new("10.0.0.1:80");
        a.ready = false;
        slices.insert("api-abc".to_owned(), vec![a, Endpoint::new("10.0.0.2:80")]);
        slices.insert("api-def".to_owned(), vec![Endpoint::new("10.0.0.1:80")]);
        let set = merge(&slices);
        let got: Vec<(&str, bool)> = set.iter().map(|e| (&*e.address, e.ready)).collect();
        assert_eq!(got, [("10.0.0.1:80", true), ("10.0.0.2:80", true)]);
    }

    #[test]
    fn credentials_never_go_over_cleartext() {
        let mut a = ApiAccess::new("http://127.0.0.1:8001/");
        assert_eq!(a.authority().unwrap(), "127.0.0.1:8001");
        assert!(a.client().is_ok());
        a.token_file = Some("/tmp/token".into());
        assert!(a.client().unwrap_err().contains("only sent over https://"));
        let tls = ApiAccess::new("https://10.0.0.1:443");
        assert!(tls.client().unwrap_err().contains("needs a CA file"));
        assert!(ApiAccess::new("ftp://x").authority().is_err());
        assert!(ApiAccess::new("https://user@x:1").authority().is_err());
        assert!(ApiAccess::new("https://x:1/path").authority().is_err());
    }

    #[test]
    fn names_say_what_is_watched() {
        assert_eq!(src(Some("http")).name(), "kubernetes:prod/api:http");
        assert_eq!(src(None).name(), "kubernetes:prod/api");
    }

    #[test]
    fn the_path_selects_the_service() {
        assert_eq!(
            src(None).path(),
            "/apis/discovery.k8s.io/v1/namespaces/prod/endpointslices\
             ?labelSelector=kubernetes.io%2Fservice-name%3Dapi"
        );
    }
}
