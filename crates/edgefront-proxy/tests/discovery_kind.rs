//! Kubernetes discovery against a real API server, in a kind cluster:
//! the steps of `discovery_k8s.rs` (scale up, a readiness flip, a slice
//! deleted, scale down and up again), first as EndpointSlices written
//! through the API and then as a Deployment's pods scaled with
//! `kubectl scale` and flipped by their readiness probes. Each change must
//! reach the proxy's balancer within 2 s.
//!
//! The tests are ignored by default: they need a cluster and `kubectl`.
//! CI's `kind` job (.github/workflows/ci.yml) creates both and runs them
//! with `--ignored`, after `scripts/kind-discovery.sh` has made the
//! namespace, a service account allowed only to list and watch
//! EndpointSlices, and the files below:
//!
//! - `EDGEFRONT_KIND_API`: the API server, `https://127.0.0.1:<port>`;
//! - `EDGEFRONT_KIND_CA`: the cluster's CA bundle;
//! - `EDGEFRONT_KIND_TOKEN`: a file holding the service account's token;
//! - `EDGEFRONT_KIND_NAMESPACE`: the namespace the tests work in.
//!
//! **How a change is timed.** Beside the proxy, `kubectl get
//! endpointslices --watch` follows the same slices, and the time it prints
//! a change is when the API server published it. A task polls the
//! balancer every millisecond and records each set it holds. A change's
//! time to the balancer runs from kubectl seeing it to the balancer first
//! holding the set it gave; it is at most 0 when the proxy saw it first.
//! For slices the test writes, the time from starting `kubectl apply` is
//! printed as well: an upper bound, since it includes kubectl's own start.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use edgefront_core::config::Config;
use edgefront_proxy::server::Proxy;
use serde_json::Value;
use tokio::sync::oneshot;

/// The bound every change must meet.
const BOUND: Duration = Duration::from_secs(2);

/// How long pods may take to start, be scheduled or go.
const SETTLE: Duration = Duration::from_secs(180);

type Set = BTreeSet<String>;

/// A log of the ready sets seen, each with when it was first seen.
type Log = Arc<Mutex<Vec<(Instant, Set)>>>;

struct Env {
    api: String,
    ca: String,
    token: String,
    namespace: String,
}

fn env() -> Env {
    let var = |name: &str| {
        std::env::var(name).unwrap_or_else(|_| {
            panic!("{name} is not set: these tests run in CI's kind job (see the file's docs)")
        })
    };
    Env {
        api: var("EDGEFRONT_KIND_API"),
        ca: var("EDGEFRONT_KIND_CA"),
        token: var("EDGEFRONT_KIND_TOKEN"),
        namespace: var("EDGEFRONT_KIND_NAMESPACE"),
    }
}

/// Runs kubectl with `args`, and `stdin` when given, failing the test on
/// a nonzero exit; returns its output.
fn kubectl(args: &[&str], stdin: Option<&str>) -> String {
    let mut child = Command::new("kubectl")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("kubectl runs");
    let mut input = child.stdin.take().unwrap();
    if let Some(text) = stdin {
        input.write_all(text.as_bytes()).unwrap();
    }
    drop(input);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "kubectl {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

async fn kubectl_async(args: Vec<String>, stdin: Option<String>) -> String {
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        kubectl(&args, stdin.as_deref())
    })
    .await
    .unwrap()
}

/// The ready `ip:port` addresses of one EndpointSlice on the port named
/// `http`, as Kubernetes defines them: ready unless the condition is
/// false.
fn slice_ready(slice: &Value) -> Vec<String> {
    let Some(port) = slice["ports"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["name"] == "http")
        .and_then(|p| p["port"].as_u64())
    else {
        return Vec::new();
    };
    let mut ready = Vec::new();
    for e in slice["endpoints"].as_array().into_iter().flatten() {
        if e["conditions"]["ready"] == Value::Bool(false) {
            continue;
        }
        for a in e["addresses"].as_array().into_iter().flatten() {
            ready.push(format!("{}:{port}", a.as_str().unwrap()));
        }
    }
    ready
}

/// Follows a Service's EndpointSlices with `kubectl get --watch`,
/// independently of the proxy, logging the ready set after each event.
struct Observer {
    log: Log,
    child: std::process::Child,
}

impl Observer {
    fn start(namespace: &str, service: &str) -> Observer {
        let mut child = Command::new("kubectl")
            .args([
                "get",
                "endpointslices",
                "-n",
                namespace,
                "-l",
                &format!("kubernetes.io/service-name={service}"),
                "--watch",
                "--output-watch-events",
                "-o",
                "json",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("kubectl runs");
        let stdout = child.stdout.take().unwrap();
        let log: Log = Arc::default();
        let into = Arc::clone(&log);
        std::thread::spawn(move || {
            let mut slices: BTreeMap<String, Vec<String>> = BTreeMap::new();
            let events = serde_json::Deserializer::from_reader(stdout).into_iter::<Value>();
            for event in events {
                let Ok(event) = event else { return };
                let at = Instant::now();
                let object = &event["object"];
                let name = object["metadata"]["name"].as_str().unwrap_or("").to_owned();
                match event["type"].as_str() {
                    Some("DELETED") => {
                        slices.remove(&name);
                    }
                    Some("ADDED" | "MODIFIED") => {
                        slices.insert(name, slice_ready(object));
                    }
                    _ => continue,
                }
                let set: Set = slices.values().flatten().cloned().collect();
                let mut log = into.lock().unwrap();
                if log.last().is_none_or(|(_, last)| *last != set) {
                    log.push((at, set));
                }
            }
        });
        Observer { log, child }
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A proxy discovering one cluster, `web`, from a Service's EndpointSlices
/// through the restricted service account, with a task logging the sets
/// its balancer holds.
struct Running {
    proxy: Arc<Proxy>,
    balanced: Log,
    _stop: oneshot::Sender<()>,
}

fn balancer_set(proxy: &Proxy) -> Set {
    proxy
        .endpoints("web")
        .unwrap_or_default()
        .iter()
        .map(|e| e.address.to_string())
        .collect()
}

fn start(env: &Env, service: &str) -> Running {
    let config = format!(
        r#"{{"listen":"127.0.0.1:0","routes":[{{"path_prefix":"/","cluster":"web"}}],
            "clusters":[{{"name":"web","policy":"round_robin","endpoints":[],
            "discovery":{{"sources":[{{"kubernetes":{{"namespace":"{}","service":"{service}",
            "port":"http","api_server":"{}","ca_file":"{}","token_file":"{}"}}}}]}}}}]}}"#,
        env.namespace, env.api, env.ca, env.token
    );
    let cfg = Config::from_json(config.as_bytes()).expect("a valid config");
    let proxy = Arc::new(Proxy::new(&cfg));
    let (stop, stopped) = oneshot::channel::<()>();
    let checks = Arc::clone(&proxy);
    tokio::spawn(async move {
        checks
            .run_checks(async move {
                let _ = stopped.await;
            })
            .await;
    });
    let balanced: Log = Arc::new(Mutex::new(vec![(Instant::now(), Set::new())]));
    let (watched, into) = (Arc::clone(&proxy), Arc::clone(&balanced));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(1));
        loop {
            tick.tick().await;
            let set = balancer_set(&watched);
            let at = Instant::now();
            let mut log = into.lock().unwrap();
            if log.last().is_some_and(|(_, last)| *last != set) {
                log.push((at, set));
            }
        }
    });
    Running {
        proxy,
        balanced,
        _stop: stop,
    }
}

/// One change's timings.
struct Timed {
    set: Set,
    /// From kubectl's watch seeing the change to the balancer holding it;
    /// negative when the proxy saw it first.
    to_balancer_ms: f64,
    /// From the start of the action to the balancer holding the set.
    from_action: Duration,
}

/// The first entry of `log` at or after `since` whose set satisfies
/// `done`, waiting up to `limit` for one.
async fn first_after(
    log: &Log,
    since: Instant,
    limit: Duration,
    what: &str,
    done: impl Fn(&Set) -> bool,
) -> (Instant, Set) {
    let deadline = Instant::now() + limit;
    loop {
        let found = log
            .lock()
            .unwrap()
            .iter()
            .find(|(at, set)| *at >= since && done(set))
            .cloned();
        if let Some(found) = found {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: no set to match within {limit:?}; last {:?}",
            log.lock().unwrap().last().map(|(_, s)| s.clone())
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Runs `action`, waits for the API server to publish the slices `done`
/// accepts, then for the balancer to hold that set, and checks the bound.
async fn step(
    p: &Running,
    observer: &Observer,
    what: &str,
    action: impl std::future::Future<Output = ()>,
    done: impl Fn(&Set) -> bool,
) -> Timed {
    let begun = Instant::now();
    action.await;
    let (seen, set) = first_after(&observer.log, begun, SETTLE, what, &done).await;
    let want = set.clone();
    let (held, _) = first_after(&p.balanced, begun, SETTLE, what, |s| *s == want).await;
    let to_balancer_ms = if held >= seen {
        held.duration_since(seen).as_secs_f64() * 1e3
    } else {
        -(seen.duration_since(held).as_secs_f64() * 1e3)
    };
    let from_action = held.duration_since(begun);
    eprintln!(
        "{what}: {} ready; kubectl watch to balancer {to_balancer_ms:.1} ms, action to balancer {:.1} ms",
        set.len(),
        from_action.as_secs_f64() * 1e3
    );
    assert!(
        to_balancer_ms < BOUND.as_secs_f64() * 1e3,
        "{what}: the balancer took {to_balancer_ms:.1} ms after the API server published it"
    );
    assert_eq!(balancer_set(&p.proxy), set, "{what}: the balancer moved on");
    Timed {
        set,
        to_balancer_ms,
        from_action,
    }
}

/// An EndpointSlice of `service` named `name`, on port `http` 8080, with
/// endpoints `(ip, ready)`, as the test writes it: managed by the test, not
/// by the EndpointSlice controller.
fn slice(name: &str, service: &str, endpoints: &[(&str, bool)]) -> String {
    let eps: Vec<Value> = endpoints
        .iter()
        .map(|(ip, ready)| {
            serde_json::json!({
                "addresses": [ip],
                "conditions": {"ready": ready},
                "zone": "zone-a",
            })
        })
        .collect();
    serde_json::json!({
        "apiVersion": "discovery.k8s.io/v1",
        "kind": "EndpointSlice",
        "metadata": {
            "name": name,
            "labels": {
                "kubernetes.io/service-name": service,
                "endpointslice.kubernetes.io/managed-by": "edgefront-tests",
            },
        },
        "addressType": "IPv4",
        "ports": [{"name": "http", "port": 8080, "protocol": "TCP"}],
        "endpoints": eps,
    })
    .to_string()
}

fn addrs(ips: &[&str]) -> Set {
    ips.iter().map(|ip| format!("{ip}:8080")).collect()
}

fn apply(ns: &str, manifest: String) -> impl std::future::Future<Output = ()> {
    let args = ["apply", "-n", ns, "-f", "-"].map(str::to_owned).to_vec();
    async move {
        kubectl_async(args, Some(manifest)).await;
    }
}

fn delete(ns: &str, kind: &str, name: &str) -> impl std::future::Future<Output = ()> {
    let args = ["delete", "-n", ns, kind, name, "--wait=false"]
        .map(str::to_owned)
        .to_vec();
    async move {
        kubectl_async(args, None).await;
    }
}

fn summary(steps: &[(&str, Timed)]) {
    eprintln!("| change | ready | kubectl watch to balancer | action to balancer |");
    eprintln!("|---|---|---|---|");
    for (what, t) in steps {
        eprintln!(
            "| {what} | {} | {:.1} ms | {:.1} ms |",
            t.set.len(),
            t.to_balancer_ms,
            t.from_action.as_secs_f64() * 1e3
        );
    }
}

/// The steps of `discovery_k8s.rs`, as EndpointSlices written through the
/// API server: each change is in the balancer within 2 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a kind cluster and kubectl; CI's kind job runs it"]
async fn slices_written_through_the_api_reach_the_balancer_within_2s() {
    let env = env();
    let ns = env.namespace.as_str();
    let svc = "slices";
    let observer = Observer::start(ns, svc);
    let p = start(&env, svc);
    let mut steps = Vec::new();

    let t = step(
        &p,
        &observer,
        "two endpoints",
        apply(
            ns,
            slice(
                "slices-a",
                svc,
                &[("10.200.0.1", true), ("10.200.0.2", true)],
            ),
        ),
        |s| *s == addrs(&["10.200.0.1", "10.200.0.2"]),
    )
    .await;
    steps.push(("two endpoints", t));
    // Another Service's slice never shows.
    kubectl_async(
        ["apply", "-n", ns, "-f", "-"].map(str::to_owned).to_vec(),
        Some(slice("other-a", "other", &[("10.200.9.1", true)])),
    )
    .await;

    let five = addrs(&[
        "10.200.0.1",
        "10.200.0.2",
        "10.200.0.3",
        "10.200.0.4",
        "10.200.0.5",
    ]);
    let t = step(
        &p,
        &observer,
        "scale to 5",
        apply(
            ns,
            slice(
                "slices-b",
                svc,
                &[
                    ("10.200.0.3", true),
                    ("10.200.0.4", true),
                    ("10.200.0.5", true),
                ],
            ),
        ),
        |s| *s == five,
    )
    .await;
    steps.push(("scale to 5", t));

    let t = step(
        &p,
        &observer,
        "readiness flip",
        apply(
            ns,
            slice(
                "slices-b",
                svc,
                &[
                    ("10.200.0.3", false),
                    ("10.200.0.4", true),
                    ("10.200.0.5", true),
                ],
            ),
        ),
        |s| *s == addrs(&["10.200.0.1", "10.200.0.2", "10.200.0.4", "10.200.0.5"]),
    )
    .await;
    steps.push(("readiness flip", t));
    // Not ready is not removed: discovery still holds it.
    let discovered = p.proxy.discovered_endpoints("web").unwrap();
    let flipped = discovered
        .iter()
        .find(|e| e.address == "10.200.0.3:8080")
        .expect("the not-ready endpoint stays discovered");
    assert!(!flipped.ready);
    assert_eq!(discovered.len(), 5);

    let t = step(
        &p,
        &observer,
        "slice deleted",
        delete(ns, "endpointslice", "slices-b"),
        |s| *s == addrs(&["10.200.0.1", "10.200.0.2"]),
    )
    .await;
    steps.push(("slice deleted", t));

    let t = step(
        &p,
        &observer,
        "scale to 1",
        apply(ns, slice("slices-a", svc, &[("10.200.0.1", true)])),
        |s| *s == addrs(&["10.200.0.1"]),
    )
    .await;
    steps.push(("scale to 1", t));

    let t = step(
        &p,
        &observer,
        "scale to 2",
        apply(
            ns,
            slice(
                "slices-a",
                svc,
                &[("10.200.0.1", true), ("10.200.0.6", true)],
            ),
        ),
        |s| *s == addrs(&["10.200.0.1", "10.200.0.6"]),
    )
    .await;
    steps.push(("scale to 2", t));
    assert!(
        p.proxy
            .discovered_endpoints("web")
            .unwrap()
            .iter()
            .all(|e| !e.address.starts_with("10.200.9.")),
        "another Service's slice showed"
    );
    summary(&steps);
}

/// A Deployment whose pods are ready while `/tmp/ready` exists, probed
/// every second, behind a Service with port `http`.
fn deployment(ns: &str, replicas: u32) -> String {
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "List",
        "items": [
            {
                "apiVersion": "v1",
                "kind": "Service",
                "metadata": {"name": "web", "namespace": ns},
                "spec": {
                    "selector": {"app": "web"},
                    "ports": [{"name": "http", "port": 8080, "targetPort": 8080}],
                },
            },
            {
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": {"name": "web", "namespace": ns},
                "spec": {
                    "replicas": replicas,
                    "selector": {"matchLabels": {"app": "web"}},
                    "template": {
                        "metadata": {"labels": {"app": "web"}},
                        "spec": {
                            "terminationGracePeriodSeconds": 1,
                            "containers": [{
                                "name": "web",
                                "image": "busybox:1.37",
                                "command": ["sh", "-c", "touch /tmp/ready && exec sleep 86400"],
                                "ports": [{"name": "http", "containerPort": 8080}],
                                "readinessProbe": {
                                    "exec": {"command": ["cat", "/tmp/ready"]},
                                    "periodSeconds": 1,
                                    "failureThreshold": 1,
                                    "successThreshold": 1,
                                },
                            }],
                        },
                    },
                },
            },
        ],
    })
    .to_string()
}

/// The running pods of the Deployment, as `(name, ip:8080)`.
async fn pods(ns: &str) -> Vec<(String, String)> {
    let out = kubectl_async(
        [
            "get",
            "pods",
            "-n",
            ns,
            "-l",
            "app=web",
            "--field-selector=status.phase=Running",
            "-o",
            "jsonpath={range .items[*]}{.metadata.name} {.status.podIP}{\"\\n\"}{end}",
        ]
        .map(str::to_owned)
        .to_vec(),
        None,
    )
    .await;
    out.lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(_, ip)| !ip.is_empty())
        .map(|(name, ip)| (name.to_owned(), format!("{ip}:8080")))
        .collect()
}

fn scale(ns: &str, replicas: u32) -> impl std::future::Future<Output = ()> {
    let args = [
        "scale".to_owned(),
        "-n".to_owned(),
        ns.to_owned(),
        "deployment/web".to_owned(),
        format!("--replicas={replicas}"),
    ]
    .to_vec();
    async move {
        kubectl_async(args, None).await;
    }
}

fn exec(ns: &str, pod: &str, command: &str) -> impl std::future::Future<Output = ()> {
    let args = ["exec", "-n", ns, pod, "--", "sh", "-c", command]
        .map(str::to_owned)
        .to_vec();
    async move {
        kubectl_async(args, None).await;
    }
}

/// A Deployment's real pods, as the EndpointSlice controller publishes
/// them: started, scaled up, flipped not ready and back by their readiness
/// probes, scaled down and up again. The time a change takes to reach the
/// balancer is counted from the API server publishing it, not from the
/// action that led to it, since pods take seconds to start or stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a kind cluster and kubectl; CI's kind job runs it"]
async fn a_deployment_scaled_and_flipped_reaches_the_balancer_within_2s() {
    let env = env();
    let ns = env.namespace.clone();
    let observer = Observer::start(&ns, "web");
    let p = start(&env, "web");
    let mut steps = Vec::new();

    let t = step(
        &p,
        &observer,
        "2 pods",
        apply(&ns, deployment(&ns, 2)),
        |s| s.len() == 2,
    )
    .await;
    steps.push(("2 pods", t));

    let t = step(&p, &observer, "scale to 5", scale(&ns, 5), |s| s.len() == 5).await;
    let five = t.set.clone();
    steps.push(("scale to 5", t));

    let (pod, ip) = pods(&ns)
        .await
        .into_iter()
        .find(|(_, ip)| five.contains(ip))
        .expect("a ready pod");
    let gone = ip.clone();
    let t = step(
        &p,
        &observer,
        "readiness flip",
        exec(&ns, &pod, "rm /tmp/ready"),
        move |s| s.len() == 4 && !s.contains(&gone),
    )
    .await;
    steps.push(("readiness flip", t));
    let flipped = p
        .proxy
        .discovered_endpoints("web")
        .unwrap()
        .into_iter()
        .find(|e| e.address == ip)
        .expect("the not-ready pod stays discovered");
    assert!(!flipped.ready);

    let back = ip.clone();
    let t = step(
        &p,
        &observer,
        "ready again",
        exec(&ns, &pod, "touch /tmp/ready"),
        move |s| s.len() == 5 && s.contains(&back),
    )
    .await;
    steps.push(("ready again", t));

    let t = step(&p, &observer, "scale to 1", scale(&ns, 1), |s| s.len() == 1).await;
    steps.push(("scale to 1", t));
    let t = step(&p, &observer, "scale to 2", scale(&ns, 2), |s| s.len() == 2).await;
    steps.push(("scale to 2", t));
    summary(&steps);
}
