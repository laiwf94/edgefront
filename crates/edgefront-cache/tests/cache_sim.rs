//! The `cache_sim` binary end to end: a trace file and a trace on standard
//! input, both formats, through every policy.

use std::io::Write;
use std::process::{Command, Stdio};

use edgefront_cache::sim::Request;
use edgefront_cache::sim::trace::oracle_record;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cache_sim"))
}

fn dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("edgefront-cache-sim-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A hot set of 100 objects taking a third of the requests, the rest a
/// scan over 20,000 one-hit objects.
fn workload() -> Vec<Request> {
    (0..60_000u64)
        .map(|i| {
            let id = if i % 3 == 0 { i % 100 } else { 1_000 + i };
            Request {
                time: i / 100,
                id,
                size: 1_000 + id % 3_000,
            }
        })
        .collect()
}

#[test]
fn replays_an_oracle_general_file() {
    let path = dir().join("trace.oracleGeneral");
    let bytes: Vec<u8> = workload().iter().flat_map(oracle_record).collect();
    std::fs::write(&path, bytes).unwrap();
    let out = bin()
        .args(["--sizes", "256KiB,1MiB,4MiB", "--output", "csv"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let rows: Vec<Vec<&str>> = text
        .lines()
        .skip(1)
        .map(|l| l.split(',').collect())
        .collect();
    assert_eq!(rows.len(), 12);
    let hits = |row: &[&str]| row[4].parse::<u64>().unwrap();
    for row in &rows {
        assert_eq!(row[3], "60000");
        // Only the hot set can hit: at most its share, less its first misses.
        assert!(hits(row) <= 20_000 - 100, "{row:?}");
    }
    // 4 MiB holds the hot set (about 250 KiB) whatever the policy, so each
    // policy misses it only on first sight.
    assert_eq!(hits(&rows[2]), 19_900, "s3fifo at 4 MiB");
    assert_eq!(hits(&rows[5]), 19_900, "lru at 4 MiB");
    assert_eq!(hits(&rows[8]), 19_900, "sieve at 4 MiB");
    assert_eq!(hits(&rows[11]), 19_900, "wtinylfu at 4 MiB");
    // At 256 KiB the scan pushes the hot set out of LRU, while S3-FIFO's
    // small queue keeps the one-hit objects away from it. SIEVE, like LRU,
    // inserts at one end: the hot set (about 250 KiB) does not fit beside
    // the scan objects still waiting for the hand, but at 1 MiB it does.
    assert!(hits(&rows[0]) > 2 * hits(&rows[3]), "{rows:?}");
    assert_eq!(hits(&rows[7]), 19_900, "sieve at 1 MiB");
    // W-TinyLFU turns the one-hit scan objects away at its door, so even
    // at 256 KiB it keeps nearly all of the hot set, and writes under 40%
    // of what LRU writes.
    assert!(hits(&rows[9]) > 19_500, "{:?}", rows[9]);
    let writes = |row: &[&str]| row[9].parse::<u64>().unwrap();
    assert!(writes(&rows[9]) * 5 < writes(&rows[3]) * 2, "{rows:?}");
}

#[test]
fn replays_csv_from_standard_input() {
    let mut csv = String::from("timestamp,obj_id,obj_size\n");
    for r in workload() {
        csv.push_str(&format!("{},/obj/{},{}\n", r.time, r.id, r.size));
    }
    let mut child = bin()
        .args([
            "--format", "csv", "-s", "4MiB", "-p", "lru", "--output", "json", "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(csv.as_bytes()));
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["trace"], "-");
    assert_eq!(v["requests"], 60_000);
    assert_eq!(v["results"][0]["hits"], 19_900);
}

#[test]
fn reports_mistakes_on_standard_error() {
    let out = bin().args(["--sizes", "lots", "t"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.starts_with("cache_sim: `lots` is not a cache size\n"),
        "{err}"
    );
    let out = bin().arg("--help").output().unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .starts_with("usage: cache_sim")
    );
}
