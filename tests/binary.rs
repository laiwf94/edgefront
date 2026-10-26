mod common;

use std::process::Command;

use common::{Binary, Scratch, TestOrigin, binary_config, handler, http, reply};

fn cachet(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_cachet"))
        .args(args)
        .env_remove("CACHET_ADMIN_TOKEN")
        .env_remove("CACHET_CLUSTER_SECRET")
        .output()
        .unwrap()
}

#[test]
fn version_and_usage() {
    let out = cachet(&["--version"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("cachet "));
    let out = cachet(&[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage"));
}

#[test]
fn check_config() {
    let dir = Scratch::new("check");
    std::fs::create_dir_all(&dir.0).unwrap();
    let good = dir.0.join("good.toml");
    std::fs::write(
        &good,
        binary_config("edge-a", "127.0.0.1:9".parse().unwrap(), ""),
    )
    .unwrap();
    let out = cachet(&["--check-config", good.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let bad = dir.0.join("bad.toml");
    std::fs::write(&bad, "node_id = \"x\"\nbogus = 1\n").unwrap();
    let out = cachet(&["--check-config", bad.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("bogus"));

    let out = cachet(&["--config", dir.0.join("missing.toml").to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read"));

    let peers = dir.0.join("peers.toml");
    let extra = "[[peers]]\nname = \"edge-b\"\nurl = \"http://127.0.0.1:1\"\n";
    std::fs::write(
        &peers,
        binary_config("edge-a", "127.0.0.1:9".parse().unwrap(), extra),
    )
    .unwrap();
    let out = cachet(&["--check-config", peers.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("CACHET_CLUSTER_SECRET"));
}

#[tokio::test]
async fn runs_logs_json_and_stops_on_sigterm() {
    let o = TestOrigin::start(handler(|_, _| {
        reply(200, &[("cache-control", "max-age=60")], "hello")
    }))
    .await;
    let dir = Scratch::new("run");
    let mut bin = Binary::start(
        &dir.0,
        &binary_config("edge-a", o.addr, ""),
        &[("CACHET_ADMIN_TOKEN", "t")],
    );
    let got = http(
        "GET",
        bin.proxy_addr,
        "/x",
        &[("x-request-id", "abc-123")],
        "",
    )
    .await;
    assert_eq!(got.body, "hello");
    assert_eq!(
        http("GET", bin.admin_addr, "/healthz", &[], "")
            .await
            .status,
        200
    );
    bin.signal("TERM");
    assert!(bin.wait().success());
    let lines = bin.drain_lines();
    let request = lines
        .iter()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("every log line is JSON"))
        .find(|v| v["fields"]["message"] == "request")
        .expect("a request log line");
    assert_eq!(request["fields"]["request_id"], "abc-123");
    assert_eq!(request["fields"]["status"], 200);
    assert_eq!(request["fields"]["outcome"], "miss");
    assert!(lines.iter().any(|l| l.contains("cachet stopped")));
}

#[tokio::test]
async fn exits_when_it_cannot_start() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap();
    let dir = Scratch::new("taken");
    std::fs::create_dir_all(&dir.0).unwrap();
    let config = dir.0.join("c.toml");
    let text = binary_config("edge-a", "127.0.0.1:9".parse().unwrap(), "").replace(
        "listen = \"127.0.0.1:0\"\nadmin",
        &format!("listen = \"{addr}\"\nadmin"),
    );
    std::fs::write(&config, text).unwrap();
    let out = cachet(&["--config", config.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("cannot listen"));
}
