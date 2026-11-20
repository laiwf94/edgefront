//! The http-cache-tests suite (github.com/http-tests/cache-tests) against
//! the real binary as a shared cache: the suite's server is the origin,
//! the binary sits in front of it, and the suite's command line sends every
//! test that is not browser-only through the binary.
//!
//! Each test is classified by the suite's own rules
//! (`tests/conformance/classify.mjs`), and the run is held to
//! `tests/conformance/allowlist.toml`, the committed expected results:
//!
//! - a required or optimal test that does not pass must be listed there,
//!   with the result it gives and the reason;
//! - a listed test must still give exactly that result, so a fix shows up
//!   as a stale entry to remove and a different failure is not hidden;
//! - every listed id must be a test the suite runs.
//!
//! Behaviour checks (`kind = "check"`, a yes or no about an optional
//! behaviour) are recorded in the summary but not judged.
//!
//! The suite is third-party Node code, so this test is ignored by default
//! and run only by the conformance CI job (.github/workflows/conformance.yml),
//! which checks the suite out at a pinned commit and runs
//! `cargo test --test conformance -- --ignored` with:
//!
//! - `CACHE_TESTS_DIR`: the suite's checkout (required);
//! - `CONFORMANCE_OUT`: where to write `results.json` (the suite's raw
//!   output), `classified.json`, `summary.md`, and the binary's and the
//!   suite server's logs, `node.log` and `server.log` (optional);
//! - `NODE`: the node binary (default `node`).
mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{Binary, Scratch, binary_config};
use serde::Deserialize;

const ALLOWLIST: &str = include_str!("conformance/allowlist.toml");

/// One test's result, as classify.mjs prints it.
#[derive(Debug, Deserialize)]
struct Classified {
    id: String,
    suite: String,
    kind: String,
    result: String,
    detail: String,
}

/// tests/conformance/allowlist.toml: `[tests.<id>]` with the result the
/// test gives and why.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Allowlist {
    tests: BTreeMap<String, Deviation>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Deviation {
    result: String,
    reason: String,
}

/// The suite's result names (resultTypes in test-engine/lib/results.mjs).
const RESULTS: [&str; 10] = [
    "pass",
    "fail",
    "optional_fail",
    "yes",
    "no",
    "setup_fail",
    "harness_fail",
    "dependency_fail",
    "retry",
    "untested",
];

/// What the run found against the allowlist.
#[derive(Debug, Default, PartialEq)]
struct Judgement {
    /// Judged tests that did not pass and are not listed: (id, result, detail).
    unexpected: Vec<(String, String, String)>,
    /// Listed tests that now give another result: (id, listed, got).
    changed: Vec<(String, String, String)>,
    /// Listed ids the suite did not run.
    unknown: Vec<String>,
    /// Listed entries that are not well formed: (id, problem).
    malformed: Vec<(String, String)>,
}

impl Judgement {
    fn passed(&self) -> bool {
        self.unexpected.is_empty()
            && self.changed.is_empty()
            && self.unknown.is_empty()
            && self.malformed.is_empty()
    }
}

fn judge(tests: &[Classified], allow: &Allowlist) -> Judgement {
    let mut j = Judgement::default();
    for (id, d) in &allow.tests {
        if !RESULTS.contains(&d.result.as_str()) || d.result == "pass" {
            j.malformed.push((
                id.clone(),
                format!("result {:?} is not a failing result", d.result),
            ));
        }
        if d.reason.trim().is_empty() {
            j.malformed.push((id.clone(), "no reason".into()));
        }
    }
    let ran: BTreeSet<&str> = tests.iter().map(|t| t.id.as_str()).collect();
    j.unknown = allow
        .tests
        .keys()
        .filter(|id| !ran.contains(id.as_str()))
        .cloned()
        .collect();
    for t in tests {
        match allow.tests.get(&t.id) {
            Some(d) if d.result != t.result => {
                j.changed
                    .push((t.id.clone(), d.result.clone(), t.result.clone()));
            }
            Some(_) => {}
            None if t.kind != "check" && t.result != "pass" => {
                j.unexpected
                    .push((t.id.clone(), t.result.clone(), t.detail.clone()));
            }
            None => {}
        }
    }
    j
}

/// The job summary: counts per kind and result, then every judged test
/// that did not pass, with its reason or the problem.
fn summary(tests: &[Classified], allow: &Allowlist, j: &Judgement, suite_commit: &str) -> String {
    let mut out = String::from("## http-cache-tests\n\n");
    out.push_str(&format!(
        "The suite at `{suite_commit}` against the binary as a shared cache: {} tests, {}.\n\n",
        tests.len(),
        if j.passed() {
            "every one as expected (`tests/conformance/allowlist.toml`)"
        } else {
            "**not as expected**"
        }
    ));
    let mut counts: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for t in tests {
        *counts.entry((&t.kind, &t.result)).or_default() += 1;
    }
    out.push_str("| Kind | Result | Tests |\n|---|---|---:|\n");
    for ((kind, result), n) in &counts {
        out.push_str(&format!("| {kind} | {result} | {n} |\n"));
    }
    if !j.unexpected.is_empty() {
        out.push_str("\n### Not passing and not in the allowlist\n\n| Test | Result | Detail |\n|---|---|---|\n");
        for (id, result, detail) in &j.unexpected {
            out.push_str(&format!("| `{id}` | {result} | {} |\n", cell(detail)));
        }
    }
    if !j.changed.is_empty() {
        out.push_str(
            "\n### Listed with another result\n\n| Test | Listed | Now |\n|---|---|---|\n",
        );
        for (id, listed, got) in &j.changed {
            out.push_str(&format!("| `{id}` | {listed} | {got} |\n"));
        }
    }
    if !j.unknown.is_empty() || !j.malformed.is_empty() {
        out.push_str("\n### Allowlist problems\n\n");
        for id in &j.unknown {
            out.push_str(&format!("- `{id}`: not a test the suite ran\n"));
        }
        for (id, problem) in &j.malformed {
            out.push_str(&format!("- `{id}`: {problem}\n"));
        }
    }
    let listed: Vec<&Classified> = tests
        .iter()
        .filter(|t| allow.tests.contains_key(&t.id))
        .collect();
    if !listed.is_empty() {
        out.push_str("\n### Allowlisted\n\n| Test | Suite | Kind | Result | Reason |\n|---|---|---|---|---|\n");
        for t in listed {
            out.push_str(&format!(
                "| `{}` | {} | {} | {} | {} |\n",
                t.id,
                t.suite,
                t.kind,
                t.result,
                cell(&allow.tests[&t.id].reason)
            ));
        }
    }
    out
}

fn cell(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let short: String = one_line.chars().take(300).collect();
    short.replace('|', "\\|")
}

/// A child process killed when dropped, with its stdout read line by line.
struct Node {
    child: Child,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Node {
    fn spawn(mut cmd: Command) -> Node {
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {cmd:?}: {e}"));
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Node { child, lines }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The status line of a GET through `addr`, if it answers.
fn status_line(addr: SocketAddr, path: &str) -> Option<String> {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(1)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut text = String::new();
    s.read_to_string(&mut text).ok()?;
    text.lines().next().map(str::to_owned)
}

fn run_to_end(mut cmd: Command, what: &str, limit: Duration) -> String {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| panic!("cannot start {what}: {e}"));
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        stdout.read_to_string(&mut text).map(|_| text)
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{what} did not finish in {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        status.success(),
        "{what} failed ({status}) after {:.0?}",
        started.elapsed()
    );
    reader.join().unwrap().expect("output is UTF-8")
}

#[test]
#[ignore = "runs the third-party http-cache-tests suite (Node); the conformance CI job runs it with CACHE_TESTS_DIR set"]
fn http_cache_tests_as_a_shared_cache() {
    let suite = PathBuf::from(
        std::env::var("CACHE_TESTS_DIR").expect("CACHE_TESTS_DIR: the http-cache-tests checkout"),
    );
    let node = std::env::var("NODE").unwrap_or_else(|_| "node".into());
    let out_dir = std::env::var("CONFORMANCE_OUT").ok().map(PathBuf::from);
    let allow: Allowlist = toml::from_str(ALLOWLIST).expect("tests/conformance/allowlist.toml");
    let suite_commit = Command::new("git")
        .arg("-C")
        .arg(&suite)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let scratch = Scratch::new("conformance");
    std::fs::create_dir_all(&scratch.0).unwrap();

    // The suite's server: the origin, serving each test's configured
    // responses and the repository's files (index.html for health checks).
    let origin_port = free_port();
    let mut server = Command::new(&node);
    server
        .arg("test-engine/server/server.mjs")
        .current_dir(&suite)
        .env("npm_config_protocol", "http")
        .env("npm_config_port", origin_port.to_string())
        .env("npm_config_pidfile", scratch.0.join("server.pid"));
    let server = Node::spawn(server);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let line = server
            .lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the suite's server did not start");
        if line.starts_with("Listening on") {
            break;
        }
    }
    let origin: SocketAddr = ([127, 0, 0, 1], origin_port).into();

    // The binary, every setting at its default but the health check.
    let bin = Binary::start(
        &scratch.0.join("node"),
        &binary_config("conformance", origin, "[health]\npath = \"/index.html\"\n"),
        &[],
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while !status_line(bin.proxy_addr, "/index.html").is_some_and(|l| l.contains(" 200 ")) {
        assert!(
            Instant::now() < deadline,
            "the binary never served the suite's index.html"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // Every test the command line runs (all but the browser-only ones).
    let mut cli = Command::new(&node);
    cli.args(["--no-warnings", "test-engine/cli.mjs"])
        .current_dir(&suite)
        .env("npm_config_base", format!("http://{}", bin.proxy_addr))
        .env("npm_config_id", "")
        .env("npm_package_config_id", "");
    let raw = run_to_end(cli, "the suite's command line", Duration::from_secs(600));
    // The binary's request log and the suite server's output, for reading
    // a failure: every line either printed while the suite ran.
    let node_log = bin.drain_lines().join("\n");
    let server_log: Vec<String> = server.lines.try_iter().collect();
    drop(bin);
    drop(server);
    let results = scratch.0.join("results.json");
    std::fs::write(&results, &raw).unwrap();

    let classify = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/conformance/classify.mjs");
    let mut cmd = Command::new(&node);
    cmd.arg(&classify).arg(&suite).arg(&results);
    let classified = run_to_end(cmd, "classify.mjs", Duration::from_secs(60));
    let tests: Vec<Classified> = serde_json::from_str(&classified).expect("classify.mjs output");
    assert!(tests.len() > 300, "only {} tests ran", tests.len());

    let j = judge(&tests, &allow);
    let text = summary(&tests, &allow, &j, &suite_commit);
    if let Some(dir) = &out_dir {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("results.json"), &raw).unwrap();
        std::fs::write(dir.join("classified.json"), &classified).unwrap();
        std::fs::write(dir.join("summary.md"), &text).unwrap();
        std::fs::write(dir.join("node.log"), &node_log).unwrap();
        std::fs::write(dir.join("server.log"), server_log.join("\n")).unwrap();
    }
    println!("{text}");
    assert!(
        j.passed(),
        "the suite's results differ from tests/conformance/allowlist.toml:\n{text}"
    );
}

#[cfg(test)]
mod judging {
    use super::*;

    fn t(id: &str, kind: &str, result: &str) -> Classified {
        Classified {
            id: id.into(),
            suite: "s".into(),
            kind: kind.into(),
            result: result.into(),
            detail: if result == "pass" {
                ""
            } else {
                "Assertion: why | it failed"
            }
            .into(),
        }
    }

    fn allow(text: &str) -> Allowlist {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn the_committed_allowlist_is_well_formed() {
        let a = allow(ALLOWLIST);
        for (id, d) in &a.tests {
            assert!(
                RESULTS.contains(&d.result.as_str()) && d.result != "pass",
                "{id}"
            );
            assert!(!d.reason.trim().is_empty(), "{id}");
        }
    }

    #[test]
    fn listed_failures_pass_and_everything_else_is_reported() {
        let a = allow(
            "[tests.known]\nresult = \"optional_fail\"\nreason = \"by design\"\n\
             [tests.fixed]\nresult = \"fail\"\nreason = \"was a bug\"\n\
             [tests.gone]\nresult = \"fail\"\nreason = \"renamed upstream\"\n",
        );
        let tests = [
            t("ok", "required", "pass"),
            t("known", "optimal", "optional_fail"),
            t("fixed", "required", "pass"),
            t("new", "required", "fail"),
            t("flaky", "optimal", "setup_fail"),
            t("asked", "check", "no"),
        ];
        let j = judge(&tests, &a);
        assert_eq!(
            j,
            Judgement {
                unexpected: vec![
                    (
                        "new".into(),
                        "fail".into(),
                        "Assertion: why | it failed".into()
                    ),
                    (
                        "flaky".into(),
                        "setup_fail".into(),
                        "Assertion: why | it failed".into()
                    ),
                ],
                changed: vec![("fixed".into(), "fail".into(), "pass".into())],
                unknown: vec!["gone".into()],
                malformed: vec![],
            }
        );
        assert!(!j.passed());
        let text = summary(&tests, &a, &j, "abc");
        assert!(text.contains("**not as expected**"), "{text}");
        assert!(
            text.contains("| `new` | fail | Assertion: why \\| it failed |"),
            "{text}"
        );
        assert!(text.contains("| `fixed` | fail | pass |"), "{text}");
        assert!(
            text.contains("- `gone`: not a test the suite ran"),
            "{text}"
        );
        assert!(
            text.contains("| `known` | s | optimal | optional_fail | by design |"),
            "{text}"
        );
        assert!(text.contains("| check | no | 1 |"), "{text}");

        let clean = [
            t("ok", "required", "pass"),
            t("known", "optimal", "optional_fail"),
        ];
        let a = allow("[tests.known]\nresult = \"optional_fail\"\nreason = \"by design\"\n");
        let j = judge(&clean, &a);
        assert!(j.passed(), "{j:?}");
        assert!(summary(&clean, &a, &j, "abc").contains("every one as expected"));
    }

    #[test]
    fn a_listed_pass_or_a_missing_reason_is_malformed() {
        let a = allow(
            "[tests.a]\nresult = \"pass\"\nreason = \"x\"\n[tests.b]\nresult = \"fail\"\nreason = \" \"\n",
        );
        let j = judge(
            &[t("a", "required", "pass"), t("b", "required", "fail")],
            &a,
        );
        assert_eq!(j.malformed.len(), 2, "{j:?}");
        assert!(toml::from_str::<Allowlist>("[tests.a]\nresult = \"fail\"\n").is_err());
        assert!(
            toml::from_str::<Allowlist>("[tests.a]\nresult = \"fail\"\nreason = \"x\"\nwhy = 1\n")
                .is_err()
        );
    }
}
