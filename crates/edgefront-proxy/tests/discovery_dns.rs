//! DNS discovery against a local DNS server: A and AAAA on a port, SRV
//! priorities, weights and glue, TTLs that set when names are looked up
//! again, a removed record gone within one TTL, names that do not exist,
//! and an outage that keeps the last good set through the merger.

#[path = "common/dns_server.rs"]
mod dns_server;

use std::time::{Duration, Instant};

use dns_server::DnsServer;
use edgefront_proxy::discovery::dns::{DnsQuery, DnsSource};
use edgefront_proxy::discovery::{Endpoint, Merger, Source, Update, UpdateKind};
use hickory_proto::rr::RecordType;
use tokio::sync::mpsc;

fn source(server: &DnsServer, query: DnsQuery) -> DnsSource {
    let mut s = DnsSource::new(query).servers(vec![server.addr]);
    s.min_interval = Duration::from_millis(100);
    s.retry = Duration::from_millis(100);
    s.timeout = Duration::from_millis(200);
    s
}

fn start(s: DnsSource) -> mpsc::Receiver<UpdateKind> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(Box::new(s).run(tx));
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

fn addresses(list: &[Endpoint]) -> Vec<&str> {
    list.iter().map(|e| &*e.address).collect()
}

fn host(name: &str, port: u16) -> DnsQuery {
    DnsQuery::Host {
        name: name.into(),
        port,
    }
}

#[tokio::test]
async fn a_and_aaaa_records_become_endpoints_on_the_port() {
    let dns = DnsServer::start().await;
    dns.set_ips("api.test", &["10.0.0.1", "10.0.0.2", "fd00::7"], 30);
    let mut rx = start(source(&dns, host("api.test", 8080)));
    let set = next_set(&mut rx).await;
    assert_eq!(
        addresses(&set),
        ["10.0.0.1:8080", "10.0.0.2:8080", "[fd00::7]:8080"]
    );
    assert!(
        set.iter()
            .all(|e| e.weight == 1 && e.priority == 0 && e.ready)
    );
}

#[tokio::test]
async fn names_are_looked_up_again_when_their_ttl_runs_out() {
    let dns = DnsServer::start().await;
    dns.set_ips("ttl.test", &["10.0.0.1"], 1);
    let mut rx = start(source(&dns, host("ttl.test", 80)));
    next_set(&mut rx).await;
    tokio::time::sleep(Duration::from_millis(3500)).await;
    // A lookup at the start and one each second after: 4, give or take
    // the one in flight. With the 100 ms floor and no TTL it would be 35.
    let a = dns.queries("ttl.test", RecordType::A);
    assert!((3..=5).contains(&a), "{a} lookups in 3.5 s with a 1 s TTL");
}

#[tokio::test]
async fn a_removed_record_is_gone_within_one_ttl() {
    let dns = DnsServer::start().await;
    dns.set_ips("shrink.test", &["10.0.0.1", "10.0.0.2"], 1);
    let mut rx = start(source(&dns, host("shrink.test", 80)));
    assert_eq!(next_set(&mut rx).await.len(), 2);
    for round in 0..3 {
        // Remove at a different point of the TTL each round.
        tokio::time::sleep(Duration::from_millis(150 + round * 300)).await;
        dns.set_ips("shrink.test", &["10.0.0.1"], 1);
        let removed = Instant::now();
        assert_eq!(addresses(&next_set(&mut rx).await), ["10.0.0.1:80"]);
        let took = removed.elapsed();
        eprintln!("round {round}: removal seen after {took:?}");
        assert!(
            took < Duration::from_secs(2),
            "seen after {took:?}, over one TTL plus one second"
        );
        dns.set_ips("shrink.test", &["10.0.0.1", "10.0.0.2"], 1);
        assert_eq!(next_set(&mut rx).await.len(), 2);
    }
}

#[tokio::test]
async fn srv_priorities_weights_and_glue_map_to_endpoints() {
    let dns = DnsServer::start().await;
    dns.set_srv(
        "_http._tcp.svc.test",
        &[
            (20, 0, 9000, "backup.test"),
            (10, 60, 8443, "main.test"),
            (10, 20, 8443, "other.test"),
            (30, 5, 80, "."),
        ],
        30,
    );
    dns.glue("main.test", &["10.0.1.1", "10.0.1.2"], 30);
    dns.set_ips("main.test", &["10.9.9.9"], 30);
    dns.set_ips("other.test", &["10.0.2.1"], 30);
    dns.set_ips("backup.test", &["10.0.3.1"], 30);
    let mut rx = start(source(
        &dns,
        DnsQuery::Srv {
            name: "_http._tcp.svc.test".into(),
        },
    ));
    let set = next_set(&mut rx).await;
    let got: Vec<(&str, u32, u8)> = set
        .iter()
        .map(|e| (&*e.address, e.weight, e.priority))
        .collect();
    assert_eq!(
        got,
        [
            ("10.0.1.1:8443", 60, 0),
            ("10.0.1.2:8443", 60, 0),
            ("10.0.2.1:8443", 20, 0),
            ("10.0.3.1:9000", 1, 1),
        ]
    );
    // main.test came from the glue, the others from their own lookups.
    assert_eq!(dns.queries("main.test", RecordType::A), 0);
    assert_eq!(dns.queries("other.test", RecordType::A), 1);
}

#[tokio::test]
async fn a_name_that_does_not_exist_is_an_empty_set_until_it_does() {
    let dns = DnsServer::start().await;
    let mut rx = start(source(&dns, host("later.test", 80)));
    assert!(next_set(&mut rx).await.is_empty());
    dns.set_ips("later.test", &["10.0.0.5"], 30);
    // Asked again after the negative TTL, one second.
    assert_eq!(addresses(&next_set(&mut rx).await), ["10.0.0.5:80"]);
    dns.set_ips("later.test", &[], 1);
    assert!(next_set(&mut rx).await.is_empty());
}

#[tokio::test]
async fn an_outage_keeps_the_last_good_set_and_is_counted() {
    let dns = DnsServer::start().await;
    dns.set_ips("flaky.test", &["10.0.0.1", "10.0.0.2"], 1);
    let s = source(&dns, host("flaky.test", 80));
    let name = s.name();
    let mut rx = start(s);
    let mut merger = Merger::new([name]);
    let apply = |m: &mut Merger, kind| m.apply(Update { source: 0, kind }, Instant::now());
    let first = next(&mut rx).await;
    assert_eq!(apply(&mut merger, first).added.len(), 2);

    dns.silence(true);
    let mut failures = 0;
    while failures < 2 {
        match next(&mut rx).await {
            kind @ UpdateKind::Failed(_) => {
                assert!(apply(&mut merger, kind).is_empty());
                failures += 1;
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }
    let status = merger.status().next().unwrap().clone();
    assert_eq!(status.failures, 2);
    assert!(status.stale);
    assert_eq!(merger.endpoints().count(), 2);

    dns.silence(false);
    let back = next(&mut rx).await;
    assert!(matches!(back, UpdateKind::Set(ref l) if l.len() == 2));
    assert!(apply(&mut merger, back).is_empty());
    assert!(!merger.status().next().unwrap().stale);
}
