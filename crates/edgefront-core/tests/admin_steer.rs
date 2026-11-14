//! steer's admin API, replayed against the Rust port: every case in
//! `testdata/admin_steer.json` was answered by steer's own handler (see
//! `legacy/steer/internal/admin/golden_test.go`), and the Rust [`Api`]
//! must give the same status, the same headers and the same body, byte
//! for byte. A request ID either side made up is `<generated>`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use edgefront_core::admin::go_json::unix;
use edgefront_core::admin::{Api, BalancerState, ClusterState, Control, EndpointState, ProxyState};
use edgefront_core::metrics;
use edgefront_core::reqid;
use http::Request;
use http_body_util::BodyExt;
use serde::Deserialize;

const GOLDEN: &str = include_str!("testdata/admin_steer.json");
const TOKEN: &str = "admin-token-0123456789";

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    token: bool,
    state: String,
    #[serde(default)]
    draining: bool,
    #[serde(default)]
    reload_error: String,
    method: String,
    target: String,
    #[serde(default)]
    header: BTreeMap<String, String>,
    status: u16,
    response_header: BTreeMap<String, String>,
    body: String,
}

struct Fixture {
    state: String,
    draining: bool,
    reload_error: String,
}

impl Control for Fixture {
    fn ready(&self) -> bool {
        !self.draining
    }

    fn state(&self) -> ProxyState {
        golden_state(&self.state)
    }

    async fn reload(&self) -> Result<u64, String> {
        if self.reload_error.is_empty() {
            Ok(8)
        } else {
            Err(self.reload_error.clone())
        }
    }

    fn metrics(&self, out: &mut String) {
        metrics::counter(out, "steer_test_total", "Test.", 1);
    }
}

fn endpoint(address: &str, weight: u64, priority: u64) -> EndpointState {
    EndpointState {
        address: address.into(),
        weight,
        priority,
        healthy: false,
        last_probe_error: String::new(),
        ejected: false,
        ejected_until: None,
        times_ejected: 0,
        last_eject_reason: String::new(),
        consecutive_gateway_failures: 0,
        in_flight: 0,
        ewma_latency_ms: 0.0,
        available: false,
    }
}

/// The same state as `goldenState` in the Go test.
fn golden_state(kind: &str) -> ProxyState {
    let loaded = unix(1_794_704_523, 450_000_000);
    match kind {
        "empty" => {
            return ProxyState {
                config_version: 1,
                loaded_at: loaded,
                clusters: vec![],
            };
        }
        "nan" => {
            return ProxyState {
                config_version: 3,
                loaded_at: loaded,
                clusters: vec![ClusterState {
                    name: "web".into(),
                    policy: "round_robin".into(),
                    in_flight: 0,
                    max_requests: 1,
                    retry_budget_balance: None,
                    balancer: BalancerState {
                        healthy_per_priority: vec![1],
                        total_per_priority: vec![1],
                        priority_load_percent: vec![100],
                        panic: false,
                    },
                    endpoints: vec![EndpointState {
                        healthy: true,
                        ewma_latency_ms: f64::NAN,
                        available: true,
                        ..endpoint("10.0.0.1:80", 1, 0)
                    }],
                }],
            };
        }
        _ => {}
    }
    ProxyState {
        config_version: 7,
        loaded_at: loaded,
        clusters: vec![
            ClusterState {
                name: "web".into(),
                policy: "least_loaded".into(),
                in_flight: 3,
                max_requests: 1024,
                retry_budget_balance: Some(12.5),
                balancer: BalancerState {
                    healthy_per_priority: vec![1, 1],
                    total_per_priority: vec![2, 1],
                    priority_load_percent: vec![50, 50],
                    panic: false,
                },
                endpoints: vec![
                    EndpointState {
                        healthy: true,
                        in_flight: 2,
                        ewma_latency_ms: 1.234,
                        available: true,
                        ..endpoint("10.0.0.1:8080", 3, 0)
                    },
                    EndpointState {
                        healthy: true,
                        ejected: true,
                        ejected_until: Some(unix(1_794_704_553, 450_000_001)),
                        times_ejected: 2,
                        last_eject_reason: "consecutive_gateway_failure".into(),
                        consecutive_gateway_failures: 5,
                        ewma_latency_ms: 0.0000005,
                        ..endpoint("10.0.0.2:8080", 1, 0)
                    },
                    EndpointState {
                        last_probe_error: r#"GET /healthz: status 503 <"down" & out>"#.into(),
                        in_flight: 1,
                        ewma_latency_ms: 250.0,
                        ..endpoint("[2001:db8::1]:8080", 1, 1)
                    },
                ],
            },
            ClusterState {
                name: "api".into(),
                policy: "maglev".into(),
                in_flight: 0,
                max_requests: 1,
                retry_budget_balance: None,
                balancer: BalancerState {
                    healthy_per_priority: vec![0],
                    total_per_priority: vec![1],
                    priority_load_percent: vec![0],
                    panic: true,
                },
                endpoints: vec![EndpointState {
                    ejected: true,
                    ejected_until: Some(unix(1_794_704_580, 0)),
                    times_ejected: 1,
                    last_eject_reason: "success_rate".into(),
                    ewma_latency_ms: 1e21,
                    ..endpoint("api.internal:443", 100, 0)
                }],
            },
        ],
    }
}

async fn run(case: &Case) -> (u16, BTreeMap<String, String>, String) {
    let fixture = Fixture {
        state: case.state.clone(),
        draining: case.draining,
        reload_error: case.reload_error.clone(),
    };
    let api = Api::new(Arc::new(fixture), if case.token { TOKEN } else { "" }).unwrap();
    assert_eq!(api.authenticates(), case.token);
    let mut req = Request::builder()
        .method(case.method.as_str())
        .uri(case.target.as_str());
    for (k, v) in &case.header {
        req = req.header(k, v);
    }
    let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();
    let resp = api.handle(req.body(()).unwrap(), peer).await;
    let status = resp.status().as_u16();
    let mut headers = BTreeMap::new();
    for name in resp.headers().keys() {
        let values: Vec<&str> = resp
            .headers()
            .get_all(name)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        headers.insert(name.as_str().to_owned(), values.join(", "));
    }
    let mut body = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    if let Some(id) = headers.get("x-request-id").cloned()
        && case.header.get("X-Request-Id") != Some(&id)
    {
        assert_eq!(id.len(), 24, "{}: generated ID {id:?}", case.name);
        assert!(reqid::valid(id.as_bytes()), "{}: {id:?}", case.name);
        body = body.replace(&id, "<generated>");
        headers.insert("x-request-id".into(), "<generated>".into());
    }
    (status, headers, body)
}

#[tokio::test]
async fn every_case_matches_steer() {
    let cases: Vec<Case> = serde_json::from_str(GOLDEN).unwrap();
    assert!(cases.len() >= 40, "the golden file lost cases");
    for case in &cases {
        let (status, headers, body) = run(case).await;
        assert_eq!(status, case.status, "{}: status", case.name);
        assert_eq!(headers, case.response_header, "{}: headers", case.name);
        assert_eq!(body, case.body, "{}: body", case.name);
    }
}

#[tokio::test]
async fn the_golden_file_covers_every_route() {
    let cases: Vec<Case> = serde_json::from_str(GOLDEN).unwrap();
    for (method, path, status) in [
        ("GET", "/healthz", 200),
        ("GET", "/readyz", 200),
        ("GET", "/readyz", 503),
        ("GET", "/metrics", 200),
        ("GET", "/metrics", 401),
        ("GET", "/clusters", 200),
        ("GET", "/clusters", 401),
        ("POST", "/reload", 200),
        ("POST", "/reload", 400),
        ("POST", "/reload", 401),
        ("GET", "/reload", 405),
        ("GET", "/nope", 404),
    ] {
        assert!(
            cases
                .iter()
                .any(|c| c.method == method && c.target == path && c.status == status),
            "no case for {method} {path} answering {status}"
        );
    }
}
