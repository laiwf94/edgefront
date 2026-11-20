//! A local authoritative DNS server for the discovery tests, built on
//! hickory-proto's message codec. It answers over UDP on 127.0.0.1 from a
//! table the test changes while it runs, counts the queries for each name
//! and type, and can be told to stop answering to simulate an outage.
//! A name with no records of the asked type gets an empty NOERROR answer
//! (NXDOMAIN when the name has no records at all) with an SOA whose
//! minimum is the negative TTL.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use hickory_proto::op::{Message, MessageType, OpCode, ResponseCode};
use hickory_proto::rr::rdata::{A, AAAA, SOA, SRV};
use hickory_proto::rr::{Name, RData, Record, RecordType};
use tokio::net::UdpSocket;

#[derive(Default)]
struct Zone {
    records: HashMap<(Name, RecordType), Vec<Record>>,
    /// Additional-section records sent with SRV answers.
    glue: HashMap<Name, Vec<Record>>,
    queries: HashMap<(Name, RecordType), u64>,
}

/// A running server. Dropping it stops the server.
pub struct DnsServer {
    pub addr: SocketAddr,
    zone: Arc<Mutex<Zone>>,
    silent: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

/// The negative TTL every empty answer carries.
pub const NEGATIVE_TTL: u32 = 1;

fn name(n: &str) -> Name {
    let mut n = Name::from_ascii(n).unwrap();
    n.set_fqdn(true);
    n.to_lowercase()
}

impl DnsServer {
    pub async fn start() -> DnsServer {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let zone = Arc::new(Mutex::new(Zone::default()));
        let silent = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(serve(socket, Arc::clone(&zone), Arc::clone(&silent)));
        DnsServer {
            addr,
            zone,
            silent,
            task,
        }
    }

    /// Replaces the A and AAAA records of `host` with `ips`, each with `ttl`.
    pub fn set_ips(&self, host: &str, ips: &[&str], ttl: u32) {
        let n = name(host);
        let mut a = Vec::new();
        let mut aaaa = Vec::new();
        for ip in ips {
            match ip.parse::<std::net::IpAddr>().unwrap() {
                std::net::IpAddr::V4(v4) => {
                    a.push(Record::from_rdata(n.clone(), ttl, RData::A(A(v4))))
                }
                std::net::IpAddr::V6(v6) => {
                    aaaa.push(Record::from_rdata(n.clone(), ttl, RData::AAAA(AAAA(v6))))
                }
            }
        }
        let mut z = self.zone.lock().unwrap();
        z.records.insert((n.clone(), RecordType::A), a);
        z.records.insert((n, RecordType::AAAA), aaaa);
    }

    /// Replaces the SRV records of `service` with `(priority, weight,
    /// port, target)` entries, each with `ttl`.
    pub fn set_srv(&self, service: &str, entries: &[(u16, u16, u16, &str)], ttl: u32) {
        let n = name(service);
        let list = entries
            .iter()
            .map(|(p, w, port, t)| {
                Record::from_rdata(n.clone(), ttl, RData::SRV(SRV::new(*p, *w, *port, name(t))))
            })
            .collect();
        self.zone
            .lock()
            .unwrap()
            .records
            .insert((n, RecordType::SRV), list);
    }

    /// Sends `host`'s A records in the additional section of every SRV
    /// answer.
    pub fn glue(&self, host: &str, ips: &[&str], ttl: u32) {
        let n = name(host);
        let list = ips
            .iter()
            .map(|ip| Record::from_rdata(n.clone(), ttl, RData::A(A(ip.parse().unwrap()))))
            .collect();
        self.zone.lock().unwrap().glue.insert(n, list);
    }

    /// Stops answering (every query times out) or starts again.
    pub fn silence(&self, on: bool) {
        self.silent.store(on, Ordering::SeqCst);
    }

    /// The queries answered so far for `host` and `kind`.
    pub fn queries(&self, host: &str, kind: RecordType) -> u64 {
        let z = self.zone.lock().unwrap();
        z.queries.get(&(name(host), kind)).copied().unwrap_or(0)
    }
}

impl Drop for DnsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(socket: UdpSocket, zone: Arc<Mutex<Zone>>, silent: Arc<AtomicBool>) {
    let mut buf = vec![0u8; 4096];
    loop {
        let Ok((len, peer)) = socket.recv_from(&mut buf).await else {
            return;
        };
        if silent.load(Ordering::SeqCst) {
            continue;
        }
        let Ok(query) = Message::from_vec(&buf[..len]) else {
            continue;
        };
        let reply = answer(&query, &zone);
        if let Ok(bytes) = reply.to_vec() {
            let _ = socket.send_to(&bytes, peer).await;
        }
    }
}

fn answer(query: &Message, zone: &Mutex<Zone>) -> Message {
    let mut reply = Message::new(query.metadata.id, MessageType::Response, OpCode::Query);
    reply.metadata.authoritative = true;
    reply.metadata.recursion_desired = query.metadata.recursion_desired;
    reply.metadata.recursion_available = true;
    let Some(q) = query.queries.first() else {
        reply.metadata.response_code = ResponseCode::FormErr;
        return reply;
    };
    reply.add_query(q.clone());
    let n = q.name().to_lowercase();
    let mut z = zone.lock().unwrap();
    *z.queries.entry((n.clone(), q.query_type())).or_default() += 1;
    let found = z
        .records
        .get(&(n.clone(), q.query_type()))
        .cloned()
        .unwrap_or_default();
    if found.is_empty() {
        let exists = z
            .records
            .iter()
            .any(|((rn, _), list)| *rn == n && !list.is_empty());
        if !exists {
            reply.metadata.response_code = ResponseCode::NXDomain;
        }
        let soa = SOA::new(
            name("ns.test."),
            name("hostmaster.test."),
            1,
            60,
            60,
            60,
            NEGATIVE_TTL,
        );
        reply.add_authority(Record::from_rdata(
            name("test."),
            NEGATIVE_TTL,
            RData::SOA(soa),
        ));
        return reply;
    }
    if q.query_type() == RecordType::SRV {
        for r in &found {
            if let RData::SRV(s) = &r.data
                && let Some(g) = z.glue.get(&s.target.to_lowercase())
            {
                reply.add_additionals(g.clone());
            }
        }
    }
    reply.add_answers(found);
    reply
}
