//! Envoy as the client: Envoy's global rate limit filter, in Docker, asks
//! edgefront's `ShouldRateLimit` about every request, and the test runs
//! envoyproxy/ratelimit's own integration scenarios against it
//! (`integration-test/scripts/` at the commit in
//! `tests/data/envoy_ratelimit/README.md`), asserting every request's
//! answer where the scripts check only some of them.
//!
//! Envoy runs the upstream `examples/envoy/proxy.yaml` with only its
//! cluster addresses changed, so the `ratelimit` cluster is edgefront's
//! listener on this host and `mock` and `multiservice` are two small
//! origins here answering as the upstream mock's two listeners do. The
//! service loads the upstream `examples/ratelimit/config/example.yaml`.
//!
//! The service's clock is the test's: it starts five seconds into a
//! minute and moves only when a scenario moves it, so a window never
//! turns over in the middle of a scenario and the reset Envoy reports is
//! exact. Each scenario's counters are its own, as in the upstream run,
//! where the scripts share one service in file order.

use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use edgefront_core::config::Http2;
use edgefront_core::h2::Settings;
use edgefront_core::listener::{Handler, ServerLimits, serve};
use edgefront_limiter::descriptor::{DescriptorConfig, DescriptorLimiter};
use edgefront_limiter::service::rls::RateLimitService;
use http_body_util::Full;
use hyper::body::Incoming;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The Envoy the benchmark's envoyproxy/ratelimit target runs
/// (`bench/targets/envoy-ratelimit/target.toml`).
const ENVOY: &str = "envoyproxy/envoy:v1.39.6@sha256:1fde8efee875e4e4f6a39a90fe49d9338e1caacce5762034ef9fb4bbb9ad6aed";

const DATA: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/envoy_ratelimit/integration"
);

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("docker {args:?}: {e}; the Envoy test needs Docker"));
    assert!(
        out.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// The host's address as a container on the default network reaches it,
/// from the Envoy image itself so no other image is pulled.
fn host_ip() -> String {
    docker(&[
        "run",
        "--rm",
        "--add-host=host.docker.internal:host-gateway",
        "--entrypoint",
        "getent",
        ENVOY,
        "hosts",
        "host.docker.internal",
    ])
    .split_whitespace()
    .next()
    .expect("host.docker.internal resolves")
    .to_owned()
}

/// edgefront's rate limit service, counting the calls it answers.
struct Rls {
    svc: RateLimitService,
    calls: AtomicUsize,
}

impl Handler for Rls {
    type Body = tonic::body::Body;

    async fn call(
        self: Arc<Self>,
        req: http::Request<Incoming>,
        _: SocketAddr,
    ) -> http::Response<tonic::body::Body> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.svc.call(req.map(tonic::body::Body::new)).await
    }
}

/// One of the upstream mock's listeners: 200 and its greeting to anything.
struct Mock(&'static str);

impl Handler for Mock {
    type Body = Full<Bytes>;

    async fn call(
        self: Arc<Self>,
        _: http::Request<Incoming>,
        _: SocketAddr,
    ) -> http::Response<Full<Bytes>> {
        http::Response::new(Full::new(Bytes::from_static(self.0.as_bytes())))
    }
}

async fn listen<H: Handler>(handler: Arc<H>, h2: bool) -> u16 {
    // Every address, since Envoy reaches it through the Docker bridge.
    let socket = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = socket.local_addr().unwrap().port();
    let limits = ServerLimits {
        http2: h2.then(|| {
            Settings::from(&Http2 {
                cleartext: true,
                ..Http2::default()
            })
        }),
        ..ServerLimits::default()
    };
    tokio::spawn(serve(socket, handler, limits, std::future::pending()));
    port
}

/// The upstream `example.yaml` without its last two descriptors, the
/// `service` ones with `quota_mode` and `metadata`, which the quota
/// scenarios use and the service does not load yet.
fn example_config() -> String {
    let full = std::fs::read_to_string(format!("{DATA}/example.yaml")).unwrap();
    let cut = full
        .find("  - key: service\n    value: service_1\n    quota_mode: true\n")
        .expect("example.yaml ends with the quota descriptors");
    assert!(
        full[cut..]
            .lines()
            .all(|l| !l.starts_with("  - key: ") || l == "  - key: service"),
        "only the service descriptors follow"
    );
    full[..cut].to_owned()
}

/// The upstream `proxy.yaml` with its three clusters' addresses pointed at
/// this host: the service's `ratelimit:8081` and the mock's
/// `envoy-mock:9999` and `envoy-mock:10001`.
fn proxy_config(host: &str, rls: u16, mock1: u16, mock2: u16) -> String {
    let yaml = std::fs::read_to_string(format!("{DATA}/proxy.yaml")).unwrap();
    let mut out = yaml.clone();
    for (from, to) in [
        (("ratelimit", 8081), rls),
        (("envoy-mock", 9999), mock1),
        (("envoy-mock", 10001), mock2),
    ] {
        let (name, port) = from;
        let old = format!(
            "address: {name}\n{pad}port_value: {port}\n",
            pad = " ".repeat(pad_of(&yaml, name))
        );
        let n = out.matches(&old).count();
        assert!(n >= 1, "proxy.yaml has {old:?}");
        out = out.replace(
            &old,
            &format!(
                "address: {host}\n{pad}port_value: {to}\n",
                pad = " ".repeat(pad_of(&yaml, name))
            ),
        );
    }
    out
}

/// The indentation of the `port_value` line after `address: name`.
fn pad_of(yaml: &str, name: &str) -> usize {
    let at = yaml
        .find(&format!("address: {name}\n"))
        .unwrap_or_else(|| panic!("proxy.yaml names {name}"));
    let next = &yaml[at..].lines().nth(1).unwrap();
    next.len() - next.trim_start().len()
}

/// The Envoy container, removed when dropped.
struct Envoy {
    name: String,
    proxy: u16,
    admin: u16,
}

impl Drop for Envoy {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .output();
    }
}

fn published(name: &str, port: u16) -> u16 {
    let out = docker(&["port", name, &format!("{port}/tcp")]);
    let first = out.lines().next().expect("a published port");
    first.rsplit(':').next().unwrap().parse().unwrap()
}

impl Envoy {
    /// Starts Envoy as the upstream compose file does (`--service-node
    /// proxy --service-cluster proxy --concurrency 1`) on `config`.
    fn start(config: &str) -> Envoy {
        let name = format!("efrls-envoy-{}", std::process::id());
        let file = std::env::temp_dir().join(format!("{name}.yaml"));
        std::fs::write(&file, config).unwrap();
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        docker(&[
            "create",
            "--name",
            &name,
            "--add-host=host.docker.internal:host-gateway",
            "-p",
            "127.0.0.1::8888",
            "-p",
            "127.0.0.1::8001",
            ENVOY,
            "envoy",
            "--service-node",
            "proxy",
            "--service-cluster",
            "proxy",
            "--config-path",
            "/etc/envoy/envoy.yaml",
            "--concurrency",
            "1",
            "--log-level",
            "info",
        ]);
        let envoy = Envoy {
            name: name.clone(),
            proxy: 0,
            admin: 0,
        };
        docker(&[
            "cp",
            &file.display().to_string(),
            &format!("{name}:/etc/envoy/envoy.yaml"),
        ]);
        let _ = std::fs::remove_file(&file);
        docker(&["start", &name]);
        let mut envoy = envoy;
        envoy.proxy = published(&name, 8888);
        envoy.admin = published(&name, 8001);
        envoy
    }

    fn logs(&self) -> String {
        let out = Command::new("docker")
            .args(["logs", "--tail", "60", &self.name])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
    }

    /// Waits for the admin API to say `LIVE`.
    async fn ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Ok(r) = get(self.admin, "/ready", &[]).await
                && r.status == 200
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Envoy did not start:\n{}",
                self.logs()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// A counter from the admin API's `/stats`. Envoy makes a filter's
    /// counter when it first counts, so one it never made is 0.
    async fn stat(&self, name: &str) -> u64 {
        let r = get(self.admin, &format!("/stats?filter=^{name}$"), &[])
            .await
            .unwrap();
        assert_eq!(r.status, 200, "{r:?}");
        r.body
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{name}: ")))
            .map_or(0, |v| v.trim().parse().unwrap())
    }
}

#[derive(Debug)]
struct Reply {
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// One HTTP/1.1 request on its own connection, as each of the scripts'
/// `curl` calls is.
async fn get(port: u16, path: &str, headers: &[(&str, &str)]) -> std::io::Result<Reply> {
    let mut tcp = TcpStream::connect(("127.0.0.1", port)).await?;
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: envoy-proxy\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    tcp.write_all(req.as_bytes()).await?;
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), tcp.read_to_end(&mut raw))
        .await
        .map_err(|_| std::io::Error::other("no answer in 10 s"))??;
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .ok_or_else(|| std::io::Error::other(format!("no header end in {raw:?}")))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let _version = parts.next();
    let status = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::other(format!("bad status line {status_line:?}")))?;
    let reason = parts.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked"));
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_owned()
    };
    Ok(Reply {
        status,
        reason,
        headers,
        body,
    })
}

/// A chunked body's data.
fn dechunk(mut rest: &str) -> std::io::Result<String> {
    let bad = |what: &str| std::io::Error::other(format!("bad chunked body: {what}"));
    let mut out = String::new();
    loop {
        let (size, after) = rest.split_once("\r\n").ok_or_else(|| bad("no size line"))?;
        let size = usize::from_str_radix(size.split(';').next().unwrap().trim(), 16)
            .map_err(|_| bad(size))?;
        if size == 0 {
            return Ok(out);
        }
        let data = after.get(..size).ok_or_else(|| bad("short chunk"))?;
        out.push_str(data);
        rest = after[size..]
            .strip_prefix("\r\n")
            .ok_or_else(|| bad("no chunk end"))?;
    }
}

/// The test's state: Envoy, the service's clock and the requests sent.
struct Run {
    envoy: Envoy,
    clock: Arc<AtomicU64>,
    sent: usize,
}

impl Run {
    /// Sends one request and asserts its status: 200 from the mock, or
    /// 429 from Envoy's filter on an `OVER_LIMIT` answer.
    async fn expect(
        &mut self,
        step: &str,
        path: &str,
        headers: &[(&str, &str)],
        status: u16,
    ) -> Reply {
        self.sent += 1;
        let r = get(self.envoy.proxy, path, headers)
            .await
            .unwrap_or_else(|e| panic!("{step}: {e}\n{}", self.envoy.logs()));
        assert_eq!(r.status, status, "{step}: GET {path} {headers:?} got {r:?}");
        match status {
            200 => assert_eq!(r.body, "Hello World from Service 1", "{step}"),
            429 => assert_eq!(r.reason, "Too Many Requests", "{step}"),
            _ => {}
        }
        r
    }

    /// Asserts Envoy's draft-03 `X-RateLimit-*` headers, which it builds
    /// from the answer's closest descriptor: its `current_limit`,
    /// `limit_remaining` and `duration_until_reset`.
    fn quota(step: &str, r: &Reply, limit: u32, window: u64, remaining: u32, reset: u64) {
        assert_eq!(
            r.header("x-ratelimit-limit"),
            Some(&*format!("{limit}, {limit};w={window}")),
            "{step}: {r:?}"
        );
        assert_eq!(
            r.header("x-ratelimit-remaining"),
            Some(&*remaining.to_string()),
            "{step}: {r:?}"
        );
        assert_eq!(
            r.header("x-ratelimit-reset"),
            Some(&*reset.to_string()),
            "{step}: {r:?}"
        );
    }

    fn advance(&self, secs: u64) {
        self.clock.fetch_add(secs, Ordering::SeqCst);
    }
}

/// `replaces.sh`, every request of it.
async fn replaces(run: &mut Run) {
    let four = |dest| {
        [
            ("foo", "bkthomps"),
            ("bar", "banned"),
            ("source_cluster", "proxy"),
            ("destination_cluster", dest),
        ]
    };
    // (foo: *), (bar: banned) is banned_limit, 0 a minute.
    run.expect("banned", "/twoheader", &four("")[..2], 429)
        .await;
    // (source_cluster: proxy), (destination_cluster: mock) is 1 a minute
    // and replaces nothing, so the banned limit still refuses.
    run.expect("banned beside mock", "/fourheader", &four("mock"), 429)
        .await;
    // (destination_cluster: override) replaces banned_limit with 2 a
    // minute.
    for (i, status) in [200, 200, 429].into_iter().enumerate() {
        run.expect(
            &format!("override {i}"),
            "/fourheader",
            &four("override"),
            status,
        )
        .await;
    }

    // Replacing leaves the replaced limit counting on its own: (bar:
    // bkthomps) is 1 a minute, replaced by (destination_cluster:
    // bkthomps) at 2.
    let mine = |dest| {
        [
            ("foo", "my_foo"),
            ("bar", "bkthomps"),
            ("source_cluster", "proxy"),
            ("destination_cluster", dest),
            ("category", "account"),
        ]
    };
    for (i, status) in [200, 200, 429].into_iter().enumerate() {
        run.expect(
            &format!("replaces bkthomps {i}"),
            "/fourheader",
            &mine("bkthomps")[..4],
            status,
        )
        .await;
    }
    // (destination_cluster: not_bkthomps) has no limit, so (bar:
    // bkthomps) applies and has one request left.
    for (i, status) in [200, 429].into_iter().enumerate() {
        run.expect(
            &format!("not_bkthomps {i}"),
            "/fourheader",
            &mine("not_bkthomps")[..4],
            status,
        )
        .await;
    }
    run.expect("bkthomps spent", "/fourheader", &mine("bkthomps")[..4], 429)
        .await;

    // (category: account) at 4 a minute replaces both bkthomps and
    // fake_name.
    for (i, status) in [200, 200, 200, 200, 429].into_iter().enumerate() {
        run.expect(
            &format!("account {i}"),
            "/fiveheader",
            &mine("fake"),
            status,
        )
        .await;
    }
    // Each replaced limit still counts for requests that do not replace
    // it: foo_2 gets bkthomps' one request, foo_3 fake_name's two.
    let foo2 = [("foo", "foo_2"), ("bar", "bkthomps")];
    run.expect("foo_2 0", "/twoheader", &foo2, 200).await;
    run.expect("foo_2 1", "/twoheader", &foo2, 429).await;
    let foo3 = [
        ("foo", "foo_3"),
        ("bar", "bar_3"),
        ("source_cluster", "proxy"),
        ("destination_cluster", "fake"),
    ];
    for (i, status) in [200, 200, 429].into_iter().enumerate() {
        run.expect(&format!("foo_3 {i}"), "/fourheader", &foo3, status)
            .await;
    }
    run.expect("account spent", "/fiveheader", &mine("fake"), 429)
        .await;
}

/// `simple-get.sh`: the happy path.
async fn simple_get(run: &mut Run) {
    let r = run
        .expect(
            "simple get",
            "/twoheader",
            &[("foo", "test"), ("baz", "shady")],
            200,
        )
        .await;
    // (foo: *), (baz: shady): 3 a minute, 55 s into the minute.
    Run::quota("simple get", &r, 3, 60, 2, 55);
}

/// `trigger-ratelimit.sh`: (foo: *), (baz: not-so-shady) at 3 a minute.
async fn trigger_ratelimit(run: &mut Run) {
    let h = [("foo", "pelle"), ("baz", "not-so-shady")];
    for remaining in [2, 1, 0] {
        let r = run.expect("within the limit", "/twoheader", &h, 200).await;
        Run::quota("within the limit", &r, 3, 60, remaining, 55);
    }
    for i in 0..2 {
        let r = run
            .expect(&format!("over the limit {i}"), "/twoheader", &h, 429)
            .await;
        Run::quota("over the limit", &r, 3, 60, 0, 55);
    }
    // As the quota scripts wait a minute for their buckets: the next
    // window admits again.
    run.advance(60);
    let r = run.expect("next window", "/twoheader", &h, 200).await;
    Run::quota("next window", &r, 3, 60, 2, 55);
}

/// `trigger-shadow-mode-key.sh`: (foo: *), (baz: shady) at 3 a minute in
/// shadow mode, so nothing is refused, and the sixth request shows none
/// left. The script means to check that remaining is 0 (its own message
/// says so) but compares a value that still ends in `\r`, so it passes
/// either way; this asserts the 0 the upstream service sends.
async fn trigger_shadow_mode_key(run: &mut Run) {
    let h = [("foo", "pelle"), ("baz", "shady")];
    for i in 0..6 {
        let r = run
            .expect(&format!("shadow {i}"), "/twoheader", &h, 200)
            .await;
        Run::quota("shadow", &r, 3, 60, 2u32.saturating_sub(i), 55);
    }
}

/// `value-included-in-stats-key-when-unspecified.sh`: (unspec: *) at 2 a
/// minute. The script's last check is a statsd counter of the upstream
/// service's, which this service does not export.
async fn value_included_when_unspecified(run: &mut Run) {
    let h = [("unspec", "unspecified_value")];
    run.expect("unspec 0", "/unspec", &h, 200).await;
    run.expect("unspec 1", "/unspec", &h, 200).await;
    run.expect("unspec 2", "/unspec", &h, 429).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn envoy_runs_the_upstream_integration_scenarios() {
    let config = example_config();
    let limiter = DescriptorLimiter::new(
        DescriptorConfig::load(&[("config/example.yaml", &config)], false).unwrap(),
        1 << 16,
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let clock = Arc::new(AtomicU64::new(now / 60 * 60 + 5));
    let rls = Arc::new(Rls {
        svc: RateLimitService::new(Arc::new(limiter)).with_clock({
            let clock = clock.clone();
            move || clock.load(Ordering::SeqCst)
        }),
        calls: AtomicUsize::new(0),
    });
    let rls_port = listen(rls.clone(), true).await;
    let mock1 = listen(Arc::new(Mock("Hello World from Service 1")), false).await;
    let mock2 = listen(Arc::new(Mock("Hello World from Service 2")), false).await;
    let host = tokio::task::spawn_blocking(host_ip).await.unwrap();
    let proxy = proxy_config(&host, rls_port, mock1, mock2);
    let envoy = tokio::task::spawn_blocking(move || Envoy::start(&proxy))
        .await
        .unwrap();
    envoy.ready().await;
    let mut run = Run {
        envoy,
        clock,
        sent: 0,
    };

    // The scripts in the order run-all.sh runs them, on one service.
    replaces(&mut run).await;
    simple_get(&mut run).await;
    trigger_ratelimit(&mut run).await;
    trigger_shadow_mode_key(&mut run).await;
    value_included_when_unspecified(&mut run).await;

    // Every answer was the service's: one call per request, and Envoy saw
    // no errors or failures to allow past (the filter fails open here, as
    // upstream configures it, so a missed call would otherwise pass). The
    // filter counts its answers on the route's cluster.
    assert_eq!(rls.calls.load(Ordering::SeqCst), run.sent);
    for stat in [
        "cluster.mock.ratelimit.error",
        "cluster.mock.ratelimit.failure_mode_allowed",
    ] {
        assert_eq!(run.envoy.stat(stat).await, 0, "{stat}");
    }
    let over = run.envoy.stat("cluster.mock.ratelimit.over_limit").await;
    let ok = run.envoy.stat("cluster.mock.ratelimit.ok").await;
    assert_eq!((ok, over), (run.sent as u64 - 13, 13));
}
