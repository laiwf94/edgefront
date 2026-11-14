//! The cache differential test (edgefront-difftest's `cache` command)
//! against this binary: run against itself it must find nothing, so a
//! divergence against cachet 0.1 is never noise from the harness, and it
//! must catch a node that behaves differently.
//!
//! Needs libfaketime (`apt install libfaketime`); `make difftest-cache`
//! runs the same harness against cachet 0.1.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use edgefront_difftest::cache::{self, cli, log, node};

const BIN: &str = env!("CARGO_BIN_EXE_edgefront");

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "edgefront-cache-difftest-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A script that changes the node's config with `sed` and then runs this
/// binary on it, or runs it with a changed environment.
fn wrapper(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\nexec {BIN} \"$@\"\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn text(b: Vec<u8>) -> String {
    String::from_utf8(b).unwrap()
}

#[test]
fn the_binary_against_itself_finds_nothing() {
    let dir = scratch("self");
    let args: Vec<String> = [
        "--requests",
        "6000",
        "--seed",
        "11",
        "--dir",
        dir.join("run").to_str().unwrap(),
        "--",
        BIN,
        BIN,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let cmd = cli::parse(&args, true, 0).unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = cli::run(&cmd, &mut out, &mut err);
    let (out, err) = (text(out), text(err));
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        out.starts_with("cache: 6000 generated requests, seed 11"),
        "{out}"
    );
    assert!(out.contains(": 0 divergent\n"), "{out}");
    // The workload reaches the paths the diff is for: hits, revalidation,
    // stale-if-error while the origin fails, and uncacheable methods.
    for kind in [
        "  hit\n",
        "  fwd=miss; stored\n",
        "  fwd=stale; stored\n",
        "  hit; fwd=stale; detail=stale-if-error\n",
        "  fwd=method\n",
        "  fwd=miss; detail=only-if-cached\n",
    ] {
        assert!(out.contains(kind), "{kind:?} missing from\n{out}");
    }
    for status in ["200=", "206=", "304=", "404=", "416=", "503="] {
        assert!(out.contains(status), "{status} missing from\n{out}");
    }
    assert!(
        !dir.join("run").exists(),
        "the scratch directory is removed"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_changed_node_is_caught() {
    let dir = scratch("changed");
    // One variant per URL and an hour less of heuristic freshness: the
    // kind of change a refactor could make by accident.
    let changed = wrapper(
        &dir,
        "changed",
        "sed -i 's/^max_variants = 8$/max_variants = 1/; s/^heuristic_max_secs = 86400$/heuristic_max_secs = 60/' \"$2\"",
    );
    let entries = log::generate(5, 4000);
    let plan = cache::Plan {
        entries: &entries,
        reference: Path::new(BIN),
        candidate: &changed,
        faketime: Path::new(node::DEFAULT_FAKETIME),
        dir: &dir.join("run"),
        keep: 5,
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let report = rt.block_on(cache::run(&plan)).unwrap();
    assert!(report.divergent > 5, "{}", report.render());
    assert_eq!(report.divergences.len(), 5);
    let paths: Vec<&str> = report
        .divergences
        .iter()
        .map(|d| d.entry.path.as_str())
        .collect();
    assert!(
        paths.iter().all(|p| p.starts_with("/vary/")
            || p.starts_with("/lang/")
            || p.starts_with("/heuristic/")),
        "{}",
        report.render()
    );
    for d in &report.divergences {
        assert_eq!(entries[d.line - 1], d.entry);
        assert!(!d.differences.is_empty());
    }
    let rendered = report.render();
    assert!(rendered.contains("cache-status: "), "{rendered}");
    assert!(rendered.contains(&format!("... and {} more\n", report.divergent - 5)));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_node_off_the_logical_clock_is_refused() {
    let dir = scratch("clock");
    let real_time = wrapper(&dir, "real-time", "unset LD_PRELOAD");
    let entries = log::generate(1, 10);
    let plan = cache::Plan {
        entries: &entries,
        reference: Path::new(BIN),
        candidate: &real_time,
        faketime: Path::new(node::DEFAULT_FAKETIME),
        dir: &dir.join("run"),
        keep: 5,
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let err = rt.block_on(cache::run(&plan)).unwrap_err().to_string();
    assert!(
        err.contains("candidate is not on the logical clock"),
        "{err}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_recorded_log_replays_through_the_command_line() {
    let dir = scratch("log");
    let host = log::HOST;
    let token = log::ADMIN_TOKEN;
    let recorded = format!(
        r#"{{"t":0,"method":"GET","path":"/page/1","headers":[["x-request-id","a"]]}}
{{"t":1,"method":"GET","path":"/page/1"}}
{{"t":1,"to":"admin","method":"POST","path":"/purge","headers":[["authorization","Bearer {token}"]],"body":"{{\"urls\":[\"http://{host}/page/1\"]}}"}}
{{"t":2,"method":"GET","path":"/page/1"}}
{{"t":40,"to":"origin","method":"POST","path":"/bump"}}
{{"t":40,"to":"origin","method":"POST","path":"/fail"}}
{{"t":41,"method":"GET","path":"/page/1"}}
{{"t":41,"to":"origin","method":"POST","path":"/recover"}}
{{"t":42,"method":"GET","path":"/page/1","headers":[["host","{host}"]]}}
{{"t":42,"to":"admin","method":"GET","path":"/healthz"}}
"#
    );
    let path = dir.join("recorded.jsonl");
    std::fs::write(&path, recorded).unwrap();
    let args: Vec<String> = [
        "--log",
        path.to_str().unwrap(),
        "--dir",
        dir.join("run").to_str().unwrap(),
        "--",
        BIN,
        BIN,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let cmd = cli::parse(&args, true, 0).unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = cli::run(&cmd, &mut out, &mut err);
    let (out, err) = (text(out), text(err));
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        out.contains(&format!("cache: log {}\n", path.display())),
        "{out}"
    );
    assert!(
        out.contains("7 requests and 3 origin events over 42 logical seconds"),
        "{out}"
    );
    // Miss, hit, purge, miss again. At t=41 the copy is stale and the
    // origin fails: without stale-if-error that is the origin's 503. After
    // recovery the revalidation finds a new version and stores it.
    assert!(out.contains("statuses: 200=6 503=1\n"), "{out}");
    assert!(out.contains("      1  hit\n"), "{out}");
    assert!(out.contains("      2  fwd=miss; stored\n"), "{out}");
    assert!(out.contains("      1  fwd=stale\n"), "{out}");
    assert!(out.contains("      1  fwd=stale; stored\n"), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A node whose port is taken before it binds it exits, and the harness
/// starts it again on fresh ports (#247). The wrapper's first start fails
/// as the binary does on `EADDRINUSE`; the second runs the binary.
#[test]
fn a_node_that_finds_its_port_taken_is_started_again() {
    let dir = scratch("taken");
    let marker = dir.join("failed-once");
    let flaky = wrapper(
        &dir,
        "flaky",
        &format!(
            "if [ ! -e {m} ]; then touch {m}; echo 'cannot listen on 127.0.0.1:1: Address already in use (os error 98)'; exit 1; fi",
            m = marker.display()
        ),
    );
    let args: Vec<String> = [
        "--requests",
        "300",
        "--seed",
        "5",
        "--dir",
        dir.join("run").to_str().unwrap(),
        "--",
        BIN,
        flaky.to_str().unwrap(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let cmd = cli::parse(&args, true, 0).unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = cli::run(&cmd, &mut out, &mut err);
    let (out, err) = (text(out), text(err));
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains(": 0 divergent\n"), "{out}");
    assert!(marker.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_divergent_run_exits_one() {
    let dir = scratch("exit");
    let changed = wrapper(
        &dir,
        "changed",
        "sed -i 's/^max_variants = 8$/max_variants = 1/' \"$2\"",
    );
    let path = dir.join("vary.jsonl");
    let lines: Vec<String> = ["gzip", "br", "gzip"]
        .iter()
        .map(|enc| {
            format!(
                r#"{{"t":0,"method":"GET","path":"/vary/1","headers":[["accept-encoding","{enc}"]]}}"#
            )
        })
        .collect();
    std::fs::write(&path, lines.join("\n")).unwrap();
    let args: Vec<String> = [
        "--log",
        path.to_str().unwrap(),
        "--dir",
        dir.join("run").to_str().unwrap(),
        "--",
        BIN,
        changed.to_str().unwrap(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let cmd = cli::parse(&args, true, 0).unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = cli::run(&cmd, &mut out, &mut err);
    let out = text(out);
    assert_eq!(code, 1, "{out}\n{}", text(err));
    // The third request is a hit on the reference, which kept both
    // variants, and a miss on the changed node, which kept one.
    assert!(out.contains(": 1 divergent\n"), "{out}");
    assert!(out.contains("line 3: "), "{out}");
    assert!(out.contains("    origin fetches: 0 vs 1\n"), "{out}");
    assert!(
        out.ends_with(&format!("cache: log {}\n", path.display())),
        "{out}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
