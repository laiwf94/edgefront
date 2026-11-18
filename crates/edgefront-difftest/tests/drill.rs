//! The failover drill's tools: `edgefront-difftest upstream` and
//! `loadgen`, run as the drill runs them, and `drill::Upstream` and
//! `drill::load` in process.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use edgefront_difftest::drill::{LoadOptions, Upstream, load};

const BIN: &str = env!("CARGO_BIN_EXE_edgefront-difftest");

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(BIN).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

fn put_state(control: &str, patch: &str) -> String {
    let mut c = TcpStream::connect(control).unwrap();
    write!(
        c,
        "PUT /state HTTP/1.1\r\nHost: c\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{patch}",
        patch.len()
    )
    .unwrap();
    let mut out = String::new();
    c.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    out
}

#[test]
fn the_drill_tools_run_as_the_shell_drill_runs_them() {
    let mut up = Command::new(BIN)
        .args([
            "upstream",
            "-name",
            "primary-a",
            "-addr",
            "127.0.0.1:0",
            "-control=127.0.0.1:0",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(up.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let listening = json(&line);
    assert_eq!(
        (&listening["msg"], &listening["name"]),
        (&"listening".into(), &"primary-a".into())
    );
    let addr = listening["addr"].as_str().unwrap().to_owned();
    let control = listening["control"].as_str().unwrap().to_owned();
    let url = format!("http://{addr}/");

    let (code, out, err) = run(&[
        "loadgen",
        "-url",
        &url,
        "-rate",
        "100",
        "-duration",
        "500ms",
        "-format",
        "json",
        "-label",
        "ok",
    ]);
    assert_eq!(code, 0, "{err}");
    let r = json(&out);
    assert_eq!(
        (&r["label"], &r["requests"], &r["ok"], &r["success_percent"]),
        (&"ok".into(), &50.into(), &50.into(), &100.into()),
        "{out}"
    );
    assert_eq!(r["failures"], serde_json::json!({}));

    // Half the requests fail once the control endpoint says so.
    put_state(&control, r#"{"error_rate":0.5}"#);
    let (code, out, _) = run(&[
        "loadgen",
        "-url",
        &url,
        "-rate",
        "200",
        "-duration",
        "1s",
        "-format",
        "row",
        "-label",
        "errors",
    ]);
    assert_eq!(code, 0);
    let cols: Vec<&str> = out.split('|').map(str::trim).collect();
    assert_eq!((cols[1], cols[2]), ("errors", "200"), "{out}");
    let success: f64 = cols[3].trim_end_matches('%').parse().unwrap();
    assert!((25.0..75.0).contains(&success), "{out}");

    // A blackhole holds every request until the client's timeout.
    put_state(&control, r#"{"error_rate":0,"blackhole":true}"#);
    let (code, out, _) = run(&[
        "loadgen",
        "-url",
        &url,
        "-rate",
        "20",
        "-duration",
        "250ms",
        "-timeout",
        "100ms",
    ]);
    assert_eq!(code, 0);
    assert!(
        out.starts_with("requests 5 in ")
            && out.contains("success 0.00%  (0 ok; failures: timeout x5)"),
        "{out}"
    );
    let p99: f64 = out
        .split("p99 ")
        .nth(1)
        .and_then(|s| s.split(' ').next())
        .unwrap()
        .parse()
        .unwrap();
    assert!(p99 >= 100.0, "{out}");

    // SIGTERM stops the upstream cleanly; afterwards nothing listens.
    let ok = Command::new("kill")
        .args(["-TERM", &up.id().to_string()])
        .status()
        .unwrap()
        .success();
    assert!(ok);
    assert!(up.wait().unwrap().success());
    let (code, out, _) = run(&["loadgen", "-url", &url, "-rate", "10", "-duration", "300ms"]);
    assert_eq!(code, 0);
    assert!(out.contains("failures: transport_error x3"), "{out}");
}

#[test]
fn the_drill_tools_refuse_bad_flags_with_exit_2() {
    let (code, _, err) = run(&["loadgen", "-rate", "5"]);
    assert_eq!(
        (code, err.as_str()),
        (2, "loadgen: -url must be an http:// URL\n")
    );
    let (code, _, err) = run(&["upstream", "-port", "1"]);
    assert_eq!(
        (code, err.as_str()),
        (2, "upstream: flag provided but not defined: -port\n")
    );
    // A busy address is reported, as `net.Listen` failing was.
    let busy = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = busy.local_addr().unwrap().to_string();
    let (code, _, err) = run(&["upstream", "-addr", &addr, "-control", "127.0.0.1:0"]);
    assert_eq!(code, 2);
    assert!(err.starts_with("upstream: "), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_drops_every_connection_and_stops_listening() {
    let up = Upstream::start("victim", "127.0.0.1:0", "127.0.0.1:0")
        .await
        .unwrap();
    assert_eq!(up.server().name(), "victim");
    let addr = up.addr();
    let mut settings = up.server().settings();
    settings.blackhole = true;
    up.server().set(settings);

    // A held request on an open connection.
    let mut held = TcpStream::connect(addr).unwrap();
    held.write_all(b"GET / HTTP/1.1\r\nHost: v\r\n\r\n")
        .unwrap();
    held.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while up.server().counts().2 == 0 {
        assert!(Instant::now() < deadline, "the request was never held");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    up.kill().await;
    // The held request ends without an answer, as when the process dies.
    let mut rest = Vec::new();
    let read = tokio::task::spawn_blocking(move || held.read_to_end(&mut rest).map(|_| rest))
        .await
        .unwrap();
    assert!(read.map_or(true, |bytes| bytes.is_empty()));
    assert!(TcpStream::connect(addr).is_err(), "still listening");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_past_max_inflight_are_dropped_and_left_out_of_the_percentiles() {
    let up = Upstream::start("slow", "127.0.0.1:0", "127.0.0.1:0")
        .await
        .unwrap();
    let mut settings = up.server().settings();
    settings.latency = Duration::from_millis(300);
    up.server().set(settings);
    let mut o = LoadOptions::new(format!("http://{}/x", up.addr()).parse().unwrap());
    o.rate = 40.0;
    o.duration = Duration::from_millis(250);
    o.max_inflight = 4;
    o.method = http::Method::POST;
    o.headers
        .push(("x-drill".parse().unwrap(), "1".parse().unwrap()));
    o.label = "slow".into();
    o.validate().unwrap();
    let r = load(&o).await;
    // Ten requests are due within 250 ms and each takes 300 ms, so only
    // the first four find a free slot.
    assert_eq!((r.requests, r.ok), (10, 4), "{r:?}");
    assert_eq!(r.failures.get("dropped"), Some(&6));
    assert!(r.p50_ms >= 300.0 && r.max_ms < 1000.0, "{r:?}");
    assert_eq!(r.label, "slow");
    assert_eq!(up.server().counts().0, 4);
}
