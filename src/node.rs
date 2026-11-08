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

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::admin::Admin;
use crate::cluster::Cluster;
use crate::config::{Config, Secrets, Settings, parse_base_url};
use crate::listener::{ServeReport, ServerLimits, serve};
use crate::origin::{self, Origin, OriginPool};
use crate::proxy::{Proxy, SystemClock};
use crate::store::Cache;

pub struct Node {
    pub proxy_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    pub proxy: Arc<Proxy>,
    stop: watch::Sender<bool>,
    servers: Vec<JoinHandle<ServeReport>>,
    background: Vec<JoinHandle<()>>,
}

async fn stopped(mut rx: watch::Receiver<bool>) {
    let _ = rx.wait_for(|stop| *stop).await;
}

/// Builds and starts a node. Fails on invalid config, missing secrets, an
/// unusable disk directory or a port that cannot be bound.
pub async fn start(config: &Config, secrets: &Secrets) -> Result<Node, String> {
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

    let bind = |addr: SocketAddr| async move {
        TcpListener::bind(addr)
            .await
            .map_err(|e| format!("cannot listen on {addr}: {e}"))
    };
    let proxy_listener = bind(config.listen).await?;
    let admin_listener = bind(config.admin_listen).await?;
    let proxy_addr = proxy_listener.local_addr().map_err(|e| e.to_string())?;
    let admin_addr = admin_listener.local_addr().map_err(|e| e.to_string())?;

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
        proxy,
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
    }
}

impl Node {
    /// Stops accepting, closes idle connections, lets open requests finish
    /// (each answered with `Connection: close`) for up to `shutdown_ms`,
    /// closes whatever is left, then writes out any queued disk writes.
    pub async fn shutdown(self) {
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
        self.proxy.cache.flush().await;
        tracing::info!("cachet stopped");
    }
}
