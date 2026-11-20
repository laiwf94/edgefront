//! The watched endpoint file: 1,000 rapid edits by atomic rename end in
//! one consistent final set, edits in place are seen, and a file that
//! breaks or disappears keeps the last good set through the merger.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use edgefront_proxy::discovery::file::FileSource;
use edgefront_proxy::discovery::{Endpoint, Merger, Source, Update, UpdateKind};
use tokio::sync::mpsc;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("discovery-file-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn contents(addresses: &[String]) -> String {
    let list: Vec<String> = addresses
        .iter()
        .map(|a| format!(r#"{{"address":"{a}"}}"#))
        .collect();
    format!(r#"{{"endpoints":[{}]}}"#, list.join(","))
}

/// Writes `data` to `path` the way config tools do: a temporary file
/// renamed over it.
fn replace(path: &Path, data: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

fn start(path: &Path) -> mpsc::Receiver<UpdateKind> {
    let (tx, rx) = mpsc::channel(64);
    let source = FileSource {
        path: path.to_owned(),
        interval: Duration::from_millis(20),
        debounce: Duration::from_millis(30),
    };
    assert!(source.name().starts_with("file:"));
    tokio::spawn(Box::new(source).run(tx));
    rx
}

async fn next(rx: &mut mpsc::Receiver<UpdateKind>) -> UpdateKind {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("an update within 5 s")
        .expect("the source is running")
}

fn set_of(kind: &UpdateKind) -> Vec<String> {
    match kind {
        UpdateKind::Set(list) => list.iter().map(|e| e.address.clone()).collect(),
        other => panic!("not a set: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thousand_rapid_edits_end_in_one_consistent_set() {
    let dir = scratch("rapid");
    let path = dir.join("endpoints.json");
    replace(&path, &contents(&["10.0.0.1:80".to_owned()]));
    let mut rx = start(&path);
    assert_eq!(set_of(&next(&mut rx).await), ["10.0.0.1:80"]);

    // 1,000 edits, each a whole set of 1 to 20 endpoints, as fast as the
    // file system takes them.
    let writer = std::thread::spawn({
        let path = path.clone();
        move || {
            let mut last = Vec::new();
            for i in 0..1000u32 {
                let n = 1 + i % 20;
                last = (0..n)
                    .map(|j| format!("10.{}.{}.{j}:8080", i / 250, i % 250))
                    .collect();
                replace(&path, &contents(&last));
            }
            last
        }
    });
    let started = Instant::now();
    let last = writer.join().unwrap();
    let wrote = started.elapsed();

    // Every update sent is a whole set from one edit, never a mix, and
    // the last one is the final edit.
    let mut updates = 0;
    let mut final_set = Vec::new();
    loop {
        let got = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        let Ok(Some(kind)) = got else { break };
        let addrs = set_of(&kind);
        let prefix: Vec<&str> = addrs[0].split('.').take(3).collect();
        assert!(
            addrs
                .iter()
                .all(|a| a.split('.').take(3).eq(prefix.iter().copied())),
            "a mixed set: {addrs:?}"
        );
        updates += 1;
        final_set = addrs;
    }
    assert_eq!(final_set, last);
    eprintln!("1,000 edits in {wrote:?} gave {updates} updates");
    assert!(updates < 1000, "{updates} updates: no debounce");
}

#[tokio::test]
async fn edits_in_place_and_failures_keep_the_last_good_set() {
    let dir = scratch("in-place");
    let path = dir.join("endpoints.json");
    std::fs::write(&path, contents(&["a.internal:80".to_owned()])).unwrap();
    let mut rx = start(&path);
    let mut merger = Merger::new(["file".to_owned()]);
    let now = Instant::now();
    let mut apply = |kind| merger.apply(Update { source: 0, kind }, now);
    apply(next(&mut rx).await);

    // In place, same inode.
    std::fs::write(
        &path,
        contents(&["a.internal:80".to_owned(), "b.internal:80".to_owned()]),
    )
    .unwrap();
    let c = apply(next(&mut rx).await);
    assert_eq!(c.added, vec![Endpoint::new("b.internal:80")]);

    // Half written: a failure, and the set stays.
    std::fs::write(&path, r#"{"endpoints":[{"addr"#).unwrap();
    let kind = next(&mut rx).await;
    assert!(
        matches!(&kind, UpdateKind::Failed(e) if e.contains("endpoints.json")),
        "{kind:?}"
    );
    assert!(apply(kind).is_empty());

    // Gone: a failure, and the set stays.
    std::fs::remove_file(&path).unwrap();
    let kind = next(&mut rx).await;
    assert!(matches!(&kind, UpdateKind::Failed(_)), "{kind:?}");
    assert!(apply(kind).is_empty());
    drop(apply);
    let addrs: Vec<_> = merger.endpoints().map(|e| e.address.clone()).collect();
    assert_eq!(addrs, ["a.internal:80", "b.internal:80"]);
    let status = merger.status().next().unwrap();
    assert_eq!((status.failures, status.stale), (2, true));

    // Back: the new set, and fresh.
    replace(&path, &contents(&["c.internal:80".to_owned()]));
    let c = merger.apply(
        Update {
            source: 0,
            kind: next(&mut rx).await,
        },
        now,
    );
    assert_eq!(c.removed, ["a.internal:80", "b.internal:80"]);
    assert_eq!(c.added, vec![Endpoint::new("c.internal:80")]);
    assert!(!merger.status().next().unwrap().stale);
}

#[tokio::test]
async fn the_source_stops_when_nobody_listens() {
    let dir = scratch("stop");
    let path = dir.join("endpoints.json");
    replace(&path, &contents(&["a:1".to_owned()]));
    let (tx, rx) = mpsc::channel(1);
    let task = tokio::spawn(Box::new(FileSource::new(&path)).run(tx));
    drop(rx);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the source ended")
        .unwrap();
}
