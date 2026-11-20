//! Starting and stopping a whole node: cache, origin pool, health checks,
//! the proxy listener and the admin listener.
//!
//! Both listeners are edgefront-core's [`listener`](crate::listener), the
//! one the proxy track serves on. Its limits come from `[timeouts]` and
//! `[limits]`: `client_header_ms` for a request head, `idle_ms` (by
//! default the same) for a silent keep-alive connection, `max_header_bytes`
//! for the head, and `shutdown_ms` for the drain.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use edgefront_cache::tier::shield::{ShieldRoute, Shields};
use edgefront_cache::tier::{Meta, Tier, TierSettings};
use edgefront_core::cluster::swim::{self, Membership, Swim};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::admin::Admin;
use crate::cluster::Cluster;
use crate::config::{Config, Secrets, Settings, parse_base_url};
use crate::listener::{ServeReport, ServerLimits, serve};
use crate::origin::{self, Origin, OriginPool};
use crate::proxy::{Proxy, SystemClock, TierLink};
use crate::store::Cache;

pub struct Node {
    pub proxy_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    /// Where SWIM listens, with `[tier]`.
    pub gossip_addr: Option<SocketAddr>,
    pub proxy: Arc<Proxy>,
    membership: Option<Membership>,
    stop: watch::Sender<bool>,
    servers: Vec<JoinHandle<ServeReport>>,
    background: Vec<JoinHandle<()>>,
}

async fn stopped(mut rx: watch::Receiver<bool>) {
    let _ = rx.wait_for(|stop| *stop).await;
}

/// The proxy and admin listeners a node serves on, already bound.
///
/// [`start_with`] takes these instead of binding `listen` and
/// `admin_listen` itself, so a caller that chose the ports (a test that
/// tells each node its peers' addresses before any node starts) never has
/// to release a port and bind it again, which lets any other socket take
/// it in between.
pub struct Listeners {
    pub proxy: TcpListener,
    pub admin: TcpListener,
    /// SWIM's UDP socket, with `[tier]`; bound from `tier.gossip_listen`
    /// when `None`.
    pub gossip: Option<UdpSocket>,
}

impl Listeners {
    /// Binds the config's `listen` and `admin_listen` addresses.
    pub async fn bind(config: &Config) -> Result<Self, String> {
        let bind = |addr: SocketAddr| async move {
            TcpListener::bind(addr)
                .await
                .map_err(|e| format!("cannot listen on {addr}: {e}"))
        };
        Ok(Listeners {
            proxy: bind(config.listen).await?,
            admin: bind(config.admin_listen).await?,
            gossip: None,
        })
    }
}

/// Builds and starts a node. Fails on invalid config, missing secrets, an
/// unusable disk directory or a port that cannot be bound.
pub async fn start(config: &Config, secrets: &Secrets) -> Result<Node, String> {
    start_on(config, secrets, None).await
}

/// Builds and starts a node on listeners that are already bound. The
/// config's `listen` and `admin_listen` are not used. Fails on invalid
/// config, missing secrets or an unusable disk directory; the listeners
/// are dropped then.
pub async fn start_with(
    config: &Config,
    secrets: &Secrets,
    listeners: Listeners,
) -> Result<Node, String> {
    start_on(config, secrets, Some(listeners)).await
}

async fn start_on(
    config: &Config,
    secrets: &Secrets,
    listeners: Option<Listeners>,
) -> Result<Node, String> {
    config.validate()?;
    config.validate_secrets(secrets)?;
    let cache = Cache::new(&config.cache_settings())
        .map_err(|e| format!("cannot open the disk tier: {e}"))?;
    let t = &config.timeouts;
    let client = origin::client(Duration::from_millis(t.connect_ms));
    let mut origins = Vec::new();
    for o in &config.origins {
        origins.push(Arc::new(Origin::new(&o.name, parse_base_url(&o.url)?)));
    }
    let pool = Arc::new(OriginPool::new(
        origins,
        client,
        Duration::from_millis(t.response_ms),
        config.health_settings(),
    ));
    let proxy = Proxy::new(
        cache,
        pool.clone(),
        config.proxy_settings(),
        Arc::new(SystemClock),
    );

    let Listeners {
        proxy: proxy_listener,
        admin: admin_listener,
        gossip,
    } = match listeners {
        Some(l) => l,
        None => Listeners::bind(config).await?,
    };
    let proxy_addr = proxy_listener.local_addr().map_err(|e| e.to_string())?;
    let admin_addr = admin_listener.local_addr().map_err(|e| e.to_string())?;

    let (membership, gossip_addr) = match &config.tier {
        Some(tc) => {
            let key = secrets
                .cluster_secret
                .clone()
                .ok_or("CACHET_CLUSTER_SECRET is required when [tier] is configured")?;
            let socket = match gossip {
                Some(s) => s,
                None => UdpSocket::bind(tc.gossip_listen)
                    .await
                    .map_err(|e| format!("cannot listen on {} (udp): {e}", tc.gossip_listen))?,
            };
            let gossip_addr = socket.local_addr().map_err(|e| e.to_string())?;
            let meta = Meta {
                http: tc.advertise.unwrap_or(proxy_addr),
                weight: tc.weight,
            };
            let settings = swim::Settings {
                period: Duration::from_millis(tc.period_ms),
                ping_timeout: Duration::from_millis((tc.period_ms * 3 / 10).max(5)),
                suspicion: Duration::from_millis(tc.suspicion_ms),
                ..swim::Settings::default()
            };
            let seed = config
                .node_id
                .bytes()
                .fold(0u64, |h, b| h.rotate_left(5) ^ u64::from(b));
            let mut s = Swim::new(
                &config.node_id,
                gossip_addr,
                &meta.to_json(),
                &key,
                settings,
                seed,
            );
            s.join(&tc.seeds);
            let membership = swim::run(s, socket);
            let shields = Shields::new(
                tc.shields
                    .iter()
                    .map(|r| ShieldRoute {
                        host: r.host.clone(),
                        path_prefix: r.path_prefix.clone(),
                        nodes: r.nodes.clone(),
                    })
                    .collect(),
            );
            let tier = Tier::new(
                TierSettings {
                    me: config.node_id.clone(),
                    key,
                    shields,
                    hot_rate: tc.hot_rate,
                    hot_window: Duration::from_secs(1),
                },
                membership.members.clone(),
            );
            proxy.enable_tier(TierLink {
                tier: Arc::new(tier),
                client: origin::client(Duration::from_millis(t.connect_ms)),
                timeout: Duration::from_millis(t.response_ms),
            });
            (Some(membership), Some(gossip_addr))
        }
        None => (None, None),
    };
    let limits = listener_limits(config);
    let (stop, rx) = watch::channel(false);
    let mut background = vec![pool.spawn_health_checks()];
    let cluster = match &secrets.cluster_secret {
        Some(key) => {
            let mut peers = Vec::new();
            for p in &config.peers {
                peers.push((p.name.clone(), parse_base_url(&p.url)?));
            }
            let client = origin::client(Duration::from_millis(t.connect_ms));
            let (cluster, tasks) = Cluster::start(
                &config.node_id,
                key.clone(),
                &peers,
                proxy.clone(),
                client,
                config.gossip_settings(),
            );
            background.extend(tasks);
            Some(cluster)
        }
        None => None,
    };
    let admin =
        Arc::new(Admin::new(proxy.clone(), secrets.admin_token.as_deref(), cluster).router());
    let servers = vec![
        tokio::spawn(serve(
            proxy_listener,
            proxy.clone(),
            limits,
            stopped(rx.clone()),
        )),
        tokio::spawn(serve(admin_listener, admin, limits, stopped(rx))),
    ];
    tracing::info!(node = %config.node_id, listen = %proxy_addr, admin = %admin_addr, origins = config.origins.len(), "cachet started");
    Ok(Node {
        proxy_addr,
        admin_addr,
        gossip_addr,
        proxy,
        membership,
        stop,
        servers,
        background,
    })
}

/// The listener limits a config asks for. The header count limit is
/// hyper's default of 100, which cachet's accept loop also had.
pub fn listener_limits(config: &Config) -> ServerLimits {
    let t = &config.timeouts;
    ServerLimits {
        header_read_timeout: Duration::from_millis(t.client_header_ms),
        idle_timeout: Duration::from_millis(t.idle_ms()),
        max_header_bytes: config.limits.max_header_bytes,
        max_headers: ServerLimits::default().max_headers,
        drain_timeout: Duration::from_millis(t.shutdown_ms),
        http2: None,
    }
}

impl Node {
    /// Stops at once, as a crash would: the listeners and SWIM stop without
    /// leaving the cluster or draining, and nothing is written out.
    pub fn abort(mut self) {
        if let Some(m) = self.membership.take() {
            drop(m);
        }
        for server in &self.servers {
            server.abort();
        }
        for task in &self.background {
            task.abort();
        }
    }

    /// Stops accepting, closes idle connections, lets open requests finish
    /// (each answered with `Connection: close`) for up to `shutdown_ms`,
    /// closes whatever is left, then writes out any queued disk writes.
    pub async fn shutdown(mut self) {
        // Leave the cluster first, so peers stop sending misses here.
        if let Some(m) = self.membership.take() {
            m.leave().await;
        }
        let _ = self.stop.send(true);
        let mut forced = 0;
        for server in self.servers {
            if let Ok(report) = server.await {
                forced += report.forced;
            }
        }
        if forced > 0 {
            tracing::warn!(
                forced,
                "closed connections still open after the drain timeout"
            );
        }
        for task in self.background {
            task.abort();
        }
        self.proxy.cache.shutdown().await;
        tracing::info!("cachet stopped");
    }
}
