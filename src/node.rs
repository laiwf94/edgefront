//! Starting and stopping a whole node: cache, origin pool, health checks,
//! the proxy listener and the admin listener.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::admin::Admin;
use crate::config::{Config, Secrets, parse_base_url};
use crate::origin::{self, Origin, OriginPool};
use crate::proxy::{Proxy, SystemClock};
use crate::server::{ServerLimits, serve};
use crate::store::Cache;

pub struct Node {
    pub proxy_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    pub proxy: Arc<Proxy>,
    stop: watch::Sender<bool>,
    servers: Vec<JoinHandle<()>>,
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

    let limits = ServerLimits {
        header_read: Duration::from_millis(t.client_header_ms),
        max_header_bytes: config.limits.max_header_bytes,
        drain: Duration::from_millis(t.shutdown_ms),
    };
    let (stop, rx) = watch::channel(false);
    let admin = Arc::new(Admin::new(proxy.clone(), secrets.admin_token.as_deref()));
    let servers = vec![
        tokio::spawn(serve(
            proxy_listener,
            proxy.clone(),
            limits,
            stopped(rx.clone()),
        )),
        tokio::spawn(serve(admin_listener, admin, limits, stopped(rx))),
    ];
    let background = vec![pool.spawn_health_checks()];
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

impl Node {
    /// Stops accepting, lets open requests finish, then writes out any
    /// queued disk writes.
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        for server in self.servers {
            let _ = server.await;
        }
        for task in self.background {
            task.abort();
        }
        self.proxy.cache.flush().await;
        tracing::info!("cachet stopped");
    }
}
