//! The delta feed over HTTP/2: a snapshot then changes, a feed killed
//! mid-stream and restarted while changes keep coming that the client
//! resumes from its token without losing or repeating one, a restarted
//! feed with no history that gets a snapshot, and a client that fell too
//! far behind.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use edgefront_proxy::discovery::delta::{DeltaSource, Feed, serve};
use edgefront_proxy::discovery::{Endpoint, Merger, Source, Update, UpdateKind};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Serves `feed` on `addr` (a free port when `None`) until the handle is
/// aborted, which drops every stream at once.
async fn feed_on(feed: &Arc<Feed>, addr: Option<SocketAddr>) -> (SocketAddr, JoinHandle<()>) {
    let listener = loop {
        match TcpListener::bind(addr.unwrap_or_else(|| "127.0.0.1:0".parse().unwrap())).await {
            Ok(l) => break l,
            // The old listener's port can take a moment to come free.
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    };
    let at = listener.local_addr().unwrap();
    let task = tokio::spawn(serve(
        listener,
        Arc::clone(feed),
        Duration::from_millis(200),
        std::future::pending(),
    ));
    (at, task)
}

fn follow(addr: SocketAddr) -> mpsc::Receiver<UpdateKind> {
    let (tx, rx) = mpsc::channel(1024);
    let mut source = DeltaSource::new(&format!("http://{addr}/v1/clusters/api"));
    source.retry = Duration::from_millis(20);
    source.idle = Duration::from_secs(2);
    tokio::spawn(Box::new(source).run(tx));
    rx
}

async fn next(rx: &mut mpsc::Receiver<UpdateKind>) -> UpdateKind {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("an update within 10 s")
        .expect("the source is running")
}

fn ep(i: u32) -> Endpoint {
    Endpoint::new(format!("10.0.{}.{}:8080", i / 250, i % 250 + 1))
}

/// Applies updates to a merger until it holds what `feed` holds, counting
/// failures; fails after 10 s.
async fn until_same(
    rx: &mut mpsc::Receiver<UpdateKind>,
    merger: &mut Merger,
    feed: &Feed,
) -> (u64, u64) {
    let (mut failures, mut updates) = (0, 0);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let want = feed.endpoints();
        let have: Vec<Endpoint> = merger.endpoints().cloned().collect();
        if have == want {
            return (updates, failures);
        }
        assert!(Instant::now() < deadline, "the client never caught up");
        let kind = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        let Ok(Some(kind)) = kind else {
            continue;
        };
        if matches!(kind, UpdateKind::Failed(_)) {
            failures += 1;
        } else {
            updates += 1;
        }
        merger.apply(Update { source: 0, kind }, Instant::now());
    }
}

#[tokio::test]
async fn a_snapshot_then_each_change() {
    let feed = Feed::new(1000);
    feed.set((0..3).map(ep).collect());
    let (addr, _task) = feed_on(&feed, None).await;
    let mut rx = follow(addr);
    match next(&mut rx).await {
        UpdateKind::Set(list) => assert_eq!(list, (0..3).map(ep).collect::<Vec<_>>()),
        other => panic!("expected a snapshot, got {other:?}"),
    }
    let mut changed = ep(1);
    changed.weight = 7;
    feed.upsert(changed.clone());
    assert_eq!(next(&mut rx).await, UpdateKind::Upsert(changed));
    feed.remove(&ep(0).address);
    assert_eq!(next(&mut rx).await, UpdateKind::Remove(ep(0).address));
    // Heartbeats keep a quiet stream alive past the client's 2 s limit.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    feed.upsert(ep(9));
    assert_eq!(next(&mut rx).await, UpdateKind::Upsert(ep(9)));
    assert_eq!(feed.starts(), (1, 0));
}

#[tokio::test]
async fn a_killed_feed_is_resumed_without_losing_a_change() {
    let feed = Feed::new(100_000);
    feed.set((0..100).map(ep).collect());
    let (addr, task) = feed_on(&feed, None).await;
    let mut rx = follow(addr);
    let mut merger = Merger::new(["delta".to_owned()]);
    until_same(&mut rx, &mut merger, &feed).await;

    // Changes stream; the feed dies after the first 300 of them, more come
    // while it is down, and it comes back on the same port.
    let writer = {
        let feed = Arc::clone(&feed);
        tokio::spawn(async move {
            for i in 0..900u32 {
                if i % 3 == 0 {
                    feed.remove(&ep(i % 100).address);
                } else {
                    let mut e = ep(100 + i);
                    e.weight = 1 + i % 5;
                    feed.upsert(e);
                }
                if i % 50 == 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    task.abort();
    let _ = task.await;
    let killed_at = feed.version();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_, _task) = feed_on(&feed, Some(addr)).await;
    writer.await.unwrap();
    let (updates, failures) = until_same(&mut rx, &mut merger, &feed).await;
    // One snapshot at the start, then only resumes.
    let (snapshots, resumes) = feed.starts();
    assert_eq!(snapshots, 1, "a snapshot was needed after the kill");
    assert!(resumes >= 1);
    assert!(failures >= 1, "the cut was not reported");
    eprintln!(
        "killed at version {killed_at} of {}; {updates} updates after the first sync, \
         {failures} failures, {resumes} resumes",
        feed.version()
    );
}

#[tokio::test]
async fn a_feed_that_lost_its_history_sends_a_snapshot() {
    let old = Feed::new(1000);
    old.set((0..10).map(ep).collect());
    let (addr, task) = feed_on(&old, None).await;
    let mut rx = follow(addr);
    let mut merger = Merger::new(["delta".to_owned()]);
    until_same(&mut rx, &mut merger, &old).await;
    task.abort();
    let _ = task.await;
    // A new control plane with a different set and its own history.
    let new = Feed::new(1000);
    new.set((5..20).map(ep).collect());
    let (_, _task) = feed_on(&new, Some(addr)).await;
    until_same(&mut rx, &mut merger, &new).await;
    assert_eq!(new.starts(), (1, 0));
}

#[tokio::test]
async fn a_client_too_far_behind_starts_again_from_a_snapshot() {
    let feed = Feed::new(10);
    feed.set((0..5).map(ep).collect());
    let (addr, task) = feed_on(&feed, None).await;
    let mut rx = follow(addr);
    let mut merger = Merger::new(["delta".to_owned()]);
    until_same(&mut rx, &mut merger, &feed).await;
    task.abort();
    let _ = task.await;
    for i in 5..60 {
        feed.upsert(ep(i));
    }
    let (_, _task) = feed_on(&feed, Some(addr)).await;
    until_same(&mut rx, &mut merger, &feed).await;
    assert_eq!(feed.starts(), (2, 0));
}
