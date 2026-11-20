//! Kubernetes discovery against a fake API server: a Service scaled up and
//! down with readiness flips, each change seen well within 2 s; a watch
//! cut off and resumed from its resource version; a compacted log that
//! forces a fresh list; and an API outage that keeps the last good set
//! through the merger.

#[path = "common/fake_kube.rs"]
mod fake_kube;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use edgefront_proxy::discovery::k8s::KubernetesSource;
use edgefront_proxy::discovery::{Endpoint, Merger, Source, Update, UpdateKind};
use fake_kube::FakeKube;
use tokio::sync::mpsc;

fn start(kube: &FakeKube, service: &str) -> mpsc::Receiver<UpdateKind> {
    let (tx, rx) = mpsc::channel(64);
    let source = KubernetesSource::new("prod", service, Some("http")).api(kube.api());
    tokio::spawn(Box::new(source).run(tx));
    rx
}

async fn next(rx: &mut mpsc::Receiver<UpdateKind>) -> UpdateKind {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("an update within 10 s")
        .expect("the source is running")
}

async fn next_set(rx: &mut mpsc::Receiver<UpdateKind>) -> Vec<Endpoint> {
    match next(rx).await {
        UpdateKind::Set(list) => list,
        other => panic!("expected a set, got {other:?}"),
    }
}

fn view(list: &[Endpoint]) -> Vec<(String, bool)> {
    let mut v: Vec<(String, bool)> = list.iter().map(|e| (e.address.clone(), e.ready)).collect();
    v.sort();
    v
}

fn ready(addrs: &[&str]) -> Vec<(String, bool)> {
    addrs.iter().map(|a| (format!("{a}:8080"), true)).collect()
}

/// Waits for the set after a change made at `at`, failing past 2 s.
async fn after(rx: &mut mpsc::Receiver<UpdateKind>, at: Instant, what: &str) -> Vec<Endpoint> {
    let set = next_set(rx).await;
    let took = at.elapsed();
    eprintln!("{what}: seen after {took:?}");
    assert!(took < Duration::from_secs(2), "{what} took {took:?}");
    set
}

#[tokio::test]
async fn a_service_scaled_up_and_down_with_readiness_flips() {
    let kube = FakeKube::start().await;
    kube.apply(
        "api-a",
        "api",
        8080,
        &[("10.1.0.1", Some(true)), ("10.1.0.2", None)],
    );
    kube.apply("other-a", "other", 8080, &[("10.9.0.1", Some(true))]);
    let mut rx = start(&kube, "api");
    let first = next_set(&mut rx).await;
    assert_eq!(view(&first), ready(&["10.1.0.1", "10.1.0.2"]));
    assert!(first.iter().all(|e| e.zone.as_deref() == Some("zone-a")));

    // Scale up: a second slice of three pods.
    let t = Instant::now();
    kube.apply(
        "api-b",
        "api",
        8080,
        &[
            ("10.1.0.3", Some(true)),
            ("10.1.0.4", Some(true)),
            ("10.1.0.5", Some(true)),
        ],
    );
    let set = after(&mut rx, t, "scale to 5").await;
    assert_eq!(
        view(&set),
        ready(&["10.1.0.1", "10.1.0.2", "10.1.0.3", "10.1.0.4", "10.1.0.5"])
    );

    // A readiness probe fails: the pod stays, not ready.
    let t = Instant::now();
    kube.apply(
        "api-b",
        "api",
        8080,
        &[
            ("10.1.0.3", Some(false)),
            ("10.1.0.4", Some(true)),
            ("10.1.0.5", Some(true)),
        ],
    );
    let set = after(&mut rx, t, "readiness flip").await;
    assert!(view(&set).contains(&("10.1.0.3:8080".to_owned(), false)));
    assert_eq!(set.len(), 5);

    // Scale down to one pod.
    let t = Instant::now();
    kube.delete("api-b");
    let set = after(&mut rx, t, "slice deleted").await;
    assert_eq!(set.len(), 2);
    let t = Instant::now();
    kube.apply("api-a", "api", 8080, &[("10.1.0.1", Some(true))]);
    let set = after(&mut rx, t, "scale to 1").await;
    assert_eq!(view(&set), ready(&["10.1.0.1"]));

    // Another Service's slices never showed.
    kube.apply("other-a", "other", 8080, &[("10.9.0.2", Some(true))]);
    let t = Instant::now();
    kube.apply(
        "api-a",
        "api",
        8080,
        &[("10.1.0.1", Some(true)), ("10.1.0.6", Some(true))],
    );
    let set = after(&mut rx, t, "scale to 2").await;
    assert_eq!(view(&set), ready(&["10.1.0.1", "10.1.0.6"]));
}

#[tokio::test]
async fn a_cut_watch_resumes_without_missing_a_change() {
    let kube = FakeKube::start().await;
    kube.apply("api-a", "api", 8080, &[("10.1.0.1", Some(true))]);
    let mut rx = start(&kube, "api");
    assert_eq!(next_set(&mut rx).await.len(), 1);
    kube.watched(1).await;
    kube.cut_watches();
    // The change is made only once the watch is back, from the version it
    // had, so it must come over the new watch.
    kube.watched(2).await;
    kube.apply(
        "api-a",
        "api",
        8080,
        &[("10.1.0.1", Some(true)), ("10.1.0.2", Some(true))],
    );
    let set = next_set(&mut rx).await;
    assert_eq!(view(&set), ready(&["10.1.0.1", "10.1.0.2"]));
    // It resumed from its resource version; it did not list again.
    assert_eq!(kube.lists.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_expired_resource_version_lists_again() {
    let kube = FakeKube::start().await;
    kube.apply("api-a", "api", 8080, &[("10.1.0.1", Some(true))]);
    let mut rx = start(&kube, "api");
    assert_eq!(next_set(&mut rx).await.len(), 1);
    kube.watched(1).await;
    kube.compact();
    kube.apply("api-a", "api", 8080, &[("10.1.0.2", Some(true))]);
    kube.compact();
    kube.cut_watches();
    let set = next_set(&mut rx).await;
    assert_eq!(view(&set), ready(&["10.1.0.2"]));
    assert!(kube.lists.load(Ordering::SeqCst) >= 2);
}

#[tokio::test]
async fn an_api_outage_keeps_the_last_good_set() {
    let kube = FakeKube::start().await;
    kube.apply(
        "api-a",
        "api",
        8080,
        &[("10.1.0.1", Some(true)), ("10.1.0.2", Some(true))],
    );
    let source = KubernetesSource::new("prod", "api", Some("http"));
    let mut merger = Merger::new([source.name()]);
    let (tx, mut rx) = mpsc::channel(64);
    tokio::spawn(Box::new(source.api(kube.api())).run(tx));
    let mut apply = |kind| merger.apply(Update { source: 0, kind }, Instant::now());
    assert_eq!(apply(next(&mut rx).await).added.len(), 2);

    kube.fail(true);
    match next(&mut rx).await {
        kind @ UpdateKind::Failed(_) => assert!(apply(kind).is_empty()),
        other => panic!("expected a failure, got {other:?}"),
    }
    let status = merger.status().next().unwrap().clone();
    assert!(status.stale);
    assert!(status.failures >= 1);
    assert_eq!(merger.endpoints().count(), 2);

    kube.fail(false);
    kube.apply("api-a", "api", 8080, &[("10.1.0.1", Some(true))]);
    let mut apply = |kind| merger.apply(Update { source: 0, kind }, Instant::now());
    loop {
        match next(&mut rx).await {
            UpdateKind::Failed(_) => continue,
            kind => {
                let change = apply(kind);
                assert_eq!(change.removed, ["10.1.0.2:8080"]);
                break;
            }
        }
    }
    assert!(!merger.status().next().unwrap().stale);
}
