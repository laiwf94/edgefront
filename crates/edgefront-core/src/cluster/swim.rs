//! SWIM membership (Das, Gupta and Motivala, 2002) with the Lifeguard-style
//! suspicion and incarnation rules every production SWIM uses.
//!
//! Each protocol period a node pings one member, chosen round-robin over a
//! shuffled list so every member is pinged once per round. No ack within the
//! ping timeout, and it asks `indirect` other members to ping the target for
//! it (ping-req). No ack by the end of the period, and the target becomes
//! *suspect*; a suspect that does not refute within the suspicion timeout is
//! *dead*. A member refutes a suspicion of itself by raising its incarnation
//! number and announcing itself alive again. Changes travel piggybacked on
//! pings and acks, each sent a bounded number of times, so dissemination
//! costs no extra messages.
//!
//! Every datagram is signed with HMAC-SHA256 over its exact bytes with the
//! cluster's shared key, and the tag is checked before the body is parsed,
//! so a node never decodes bytes from outside the cluster.
//!
//! [`Swim`] is the protocol as a pure state machine over a millisecond
//! clock: [`Swim::tick`] and [`Swim::receive`] return the datagrams to send.
//! Tests drive it over a simulated network with loss; [`run`] drives it over
//! a tokio UDP socket.

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ring::hmac;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Largest datagram sent or accepted, under a typical 1,500-byte MTU.
pub const MAX_DATAGRAM: usize = 1400;
const TAG_LEN: usize = 32;

/// A member's state as this node believes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Alive,
    Suspect,
    Dead,
}

/// One member of the cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// The node's name, unique in the cluster.
    pub id: String,
    /// Where its SWIM socket listens.
    pub addr: SocketAddr,
    /// What the node tells others about itself (its HTTP address and
    /// weight, say); opaque here.
    pub meta: String,
    pub state: State,
    pub incarnation: u64,
}

/// Protocol timing and sizes.
#[derive(Debug, Clone)]
pub struct Settings {
    /// One protocol period: a ping to one member.
    pub period: Duration,
    /// How long to wait for an ack before asking others (ping-req).
    pub ping_timeout: Duration,
    /// Members asked to ping a target that did not answer.
    pub indirect: usize,
    /// How long a suspect has to refute before it is declared dead.
    pub suspicion: Duration,
    /// Each change is piggybacked this many times the log2 of the cluster
    /// size (rounded up, at least once).
    pub retransmit_mult: u32,
    /// Changes piggybacked on one message at most.
    pub max_piggyback: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            period: Duration::from_millis(250),
            ping_timeout: Duration::from_millis(80),
            indirect: 3,
            suspicion: Duration::from_millis(1000),
            retransmit_mult: 3,
            max_piggyback: 8,
        }
    }
}

/// What goes on the wire, inside the signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Wire {
    /// The sender's id.
    from: String,
    msg: Msg,
    /// Membership changes piggybacked on the message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    updates: Vec<Member>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Msg {
    Ping {
        seq: u64,
    },
    Ack {
        seq: u64,
    },
    /// Ping `target` and send me its ack with `seq`.
    PingReq {
        seq: u64,
        target: String,
    },
    /// An ack relayed for a ping-req: `target` answered.
    IndirectAck {
        seq: u64,
        target: String,
    },
}

/// A datagram to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub to: SocketAddr,
    pub bytes: Vec<u8>,
}

/// A probe of one member in the current period.
#[derive(Debug, Clone)]
struct Probe {
    target: String,
    seq: u64,
    sent_at: u64,
    indirect_sent: bool,
    acked: bool,
}

/// A ping-req this node is serving for someone else.
#[derive(Debug, Clone)]
struct Relay {
    requester: SocketAddr,
    requester_seq: u64,
    target: String,
    expires: u64,
}

/// The membership protocol of one node.
pub struct Swim {
    me: Member,
    key: hmac::Key,
    settings: Settings,
    members: BTreeMap<String, Member>,
    /// When each suspect was first suspected, in ms.
    suspected_at: BTreeMap<String, u64>,
    /// Changes to piggyback, with how many times each was sent.
    queue: VecDeque<(Member, u32)>,
    order: Vec<String>,
    next: usize,
    probe: Option<Probe>,
    next_period: u64,
    relays: Vec<Relay>,
    seq: u64,
    rng: u64,
    /// Seeds pinged until one answers.
    seeds: Vec<SocketAddr>,
    joined: bool,
}

/// Why a datagram was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    TooShort,
    TooLarge,
    BadSignature,
    Malformed,
}

impl Swim {
    /// A node named `id` listening on `addr`, with the cluster `key`. `seed`
    /// makes member selection deterministic in tests.
    pub fn new(
        id: &str,
        addr: SocketAddr,
        meta: &str,
        key: &[u8],
        settings: Settings,
        seed: u64,
    ) -> Swim {
        let me = Member {
            id: id.to_owned(),
            addr,
            meta: meta.to_owned(),
            state: State::Alive,
            incarnation: 0,
        };
        let mut swim = Swim {
            me: me.clone(),
            key: hmac::Key::new(hmac::HMAC_SHA256, key),
            settings,
            members: BTreeMap::new(),
            suspected_at: BTreeMap::new(),
            queue: VecDeque::new(),
            order: Vec::new(),
            next: 0,
            probe: None,
            next_period: 0,
            relays: Vec::new(),
            seq: 0,
            rng: seed ^ 0x9e37_79b9_7f4a_7c15,
            seeds: Vec::new(),
            joined: false,
        };
        swim.enqueue(me);
        swim
    }

    /// Addresses to contact until one answers, to join a cluster.
    pub fn join(&mut self, seeds: &[SocketAddr]) {
        self.seeds = seeds
            .iter()
            .copied()
            .filter(|a| *a != self.me.addr)
            .collect();
        self.joined = self.seeds.is_empty();
    }

    /// This node as it announces itself.
    pub fn me(&self) -> &Member {
        &self.me
    }

    /// Every other member this node knows of, in any state.
    pub fn members(&self) -> impl Iterator<Item = &Member> {
        self.members.values()
    }

    /// The members believed alive (suspects included, as SWIM does until
    /// they are confirmed dead), this node first.
    pub fn alive(&self) -> Vec<Member> {
        let mut out = vec![self.me.clone()];
        out.extend(
            self.members
                .values()
                .filter(|m| m.state != State::Dead)
                .cloned(),
        );
        out
    }

    fn random(&mut self) -> u64 {
        // splitmix64
        self.rng = self.rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn retransmits(&self) -> u32 {
        let n = self.members.len() as u32 + 1;
        let log = 32 - n.leading_zeros();
        self.settings.retransmit_mult * log.max(1)
    }

    fn enqueue(&mut self, m: Member) {
        self.queue.retain(|(q, _)| q.id != m.id);
        self.queue.push_front((m, 0));
    }

    fn piggyback(&mut self) -> Vec<Member> {
        let limit = self.retransmits();
        let mut out = Vec::new();
        // The least-sent changes first.
        let mut idx: Vec<usize> = (0..self.queue.len()).collect();
        idx.sort_by_key(|&i| self.queue[i].1);
        for i in idx.into_iter().take(self.settings.max_piggyback) {
            self.queue[i].1 += 1;
            out.push(self.queue[i].0.clone());
        }
        self.queue.retain(|(_, sent)| *sent < limit);
        out
    }

    fn encode(&mut self, msg: Msg) -> Vec<u8> {
        let mut updates = self.piggyback();
        loop {
            let wire = Wire {
                from: self.me.id.clone(),
                msg: msg.clone(),
                updates: updates.clone(),
            };
            let body = serde_json::to_vec(&wire).expect("wire messages serialize");
            if body.len() + TAG_LEN <= MAX_DATAGRAM || updates.is_empty() {
                let tag = hmac::sign(&self.key, &body);
                let mut out = Vec::with_capacity(TAG_LEN + body.len());
                out.extend_from_slice(tag.as_ref());
                out.extend_from_slice(&body);
                return out;
            }
            // Too big: drop the last change and try again; it stays queued.
            updates.pop();
        }
    }

    fn decode(&self, bytes: &[u8]) -> Result<Wire, Refused> {
        if bytes.len() > MAX_DATAGRAM {
            return Err(Refused::TooLarge);
        }
        if bytes.len() <= TAG_LEN {
            return Err(Refused::TooShort);
        }
        let (tag, body) = bytes.split_at(TAG_LEN);
        hmac::verify(&self.key, body, tag).map_err(|_| Refused::BadSignature)?;
        serde_json::from_slice(body).map_err(|_| Refused::Malformed)
    }

    fn send(&mut self, to: SocketAddr, msg: Msg) -> Outgoing {
        Outgoing {
            to,
            bytes: self.encode(msg),
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Applies one membership change, by the SWIM precedence rules. Returns
    /// whether it changed anything (and so is passed on).
    fn apply(&mut self, m: Member, now: u64) -> bool {
        if m.id == self.me.id {
            // Someone believes this node suspect or dead: refute it.
            if m.state != State::Alive && m.incarnation >= self.me.incarnation {
                self.me.incarnation = m.incarnation + 1;
                let me = self.me.clone();
                self.enqueue(me);
            }
            return false;
        }
        let newer = match self.members.get(&m.id) {
            None => m.state != State::Dead,
            Some(old) => match (m.state, old.state) {
                (State::Alive, State::Alive | State::Suspect | State::Dead) => {
                    m.incarnation > old.incarnation
                }
                (State::Suspect, State::Alive) => m.incarnation >= old.incarnation,
                (State::Suspect, State::Suspect) => m.incarnation > old.incarnation,
                (State::Suspect, State::Dead) => m.incarnation > old.incarnation,
                (State::Dead, State::Alive | State::Suspect) => m.incarnation >= old.incarnation,
                (State::Dead, State::Dead) => false,
            },
        };
        if !newer {
            return false;
        }
        match m.state {
            State::Suspect => {
                self.suspected_at.entry(m.id.clone()).or_insert(now);
            }
            _ => {
                self.suspected_at.remove(&m.id);
            }
        }
        if !self.order.contains(&m.id) && m.state != State::Dead {
            // A new member goes in at a random place in the round.
            let at = (self.random() as usize) % (self.order.len() + 1);
            self.order.insert(at, m.id.clone());
        }
        self.members.insert(m.id.clone(), m.clone());
        self.enqueue(m);
        true
    }

    fn suspect(&mut self, id: &str, now: u64) {
        if let Some(m) = self.members.get(id)
            && m.state == State::Alive
        {
            let s = Member {
                state: State::Suspect,
                ..m.clone()
            };
            self.apply(s, now);
        }
    }

    /// The member to probe this period: the next in a shuffled round over
    /// the live members, reshuffled when the round ends.
    fn next_target(&mut self) -> Option<String> {
        self.order
            .retain(|id| self.members.get(id).is_some_and(|m| m.state != State::Dead));
        if self.order.is_empty() {
            return None;
        }
        if self.next >= self.order.len() {
            self.next = 0;
            for i in (1..self.order.len()).rev() {
                let j = (self.random() as usize) % (i + 1);
                self.order.swap(i, j);
            }
        }
        let id = self.order[self.next].clone();
        self.next += 1;
        Some(id)
    }

    /// Advances the protocol to `now` (ms): starts a period, sends ping-reqs
    /// for an unanswered ping, suspects, and declares suspects dead.
    pub fn tick(&mut self, now: u64) -> Vec<Outgoing> {
        let mut out = Vec::new();
        // Suspects past their timeout are dead.
        let suspicion = self.settings.suspicion.as_millis() as u64;
        let expired: Vec<String> = self
            .suspected_at
            .iter()
            .filter(|(_, at)| now.saturating_sub(**at) >= suspicion)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            if let Some(m) = self.members.get(&id).cloned() {
                self.apply(
                    Member {
                        state: State::Dead,
                        ..m
                    },
                    now,
                );
            }
            self.suspected_at.remove(&id);
        }
        self.relays.retain(|r| r.expires > now);
        // An unanswered ping past its timeout: ask others.
        let ping_timeout = self.settings.ping_timeout.as_millis() as u64;
        if let Some(p) = self.probe.clone()
            && !p.acked
            && !p.indirect_sent
            && now.saturating_sub(p.sent_at) >= ping_timeout
        {
            let mut helpers: Vec<SocketAddr> = self
                .members
                .values()
                .filter(|m| m.state == State::Alive && m.id != p.target)
                .map(|m| m.addr)
                .collect();
            for i in (1..helpers.len()).rev() {
                let j = (self.random() as usize) % (i + 1);
                helpers.swap(i, j);
            }
            helpers.truncate(self.settings.indirect);
            for h in helpers {
                let msg = Msg::PingReq {
                    seq: p.seq,
                    target: p.target.clone(),
                };
                out.push(self.send(h, msg));
            }
            if let Some(probe) = self.probe.as_mut() {
                probe.indirect_sent = true;
            }
        }
        if now < self.next_period {
            return out;
        }
        let period = self.settings.period.as_millis() as u64;
        self.next_period = now + period;
        // The period ended: an unanswered probe makes its target suspect.
        if let Some(p) = self.probe.take()
            && !p.acked
        {
            self.suspect(&p.target, now);
        }
        if !self.joined {
            let seeds = self.seeds.clone();
            for s in seeds {
                let seq = self.next_seq();
                out.push(self.send(s, Msg::Ping { seq }));
            }
        }
        if let Some(target) = self.next_target() {
            let addr = self.members[&target].addr;
            let seq = self.next_seq();
            self.probe = Some(Probe {
                target,
                seq,
                sent_at: now,
                indirect_sent: false,
                acked: false,
            });
            out.push(self.send(addr, Msg::Ping { seq }));
        }
        out
    }

    /// Handles a datagram from `from` at `now` (ms).
    pub fn receive(
        &mut self,
        now: u64,
        from: SocketAddr,
        bytes: &[u8],
    ) -> Result<Vec<Outgoing>, Refused> {
        let wire = self.decode(bytes)?;
        self.joined = true;
        let known = self.members.contains_key(&wire.from);
        for m in wire.updates {
            self.apply(m, now);
        }
        let sender = self.members.get(&wire.from).map(|m| m.state);
        // A member heard from for the first time, or one this node holds
        // dead (a restarted process), learns the whole membership from what
        // is piggybacked next, not only the latest changes. The dead entry
        // goes first, so a restarted process refutes it with a higher
        // incarnation.
        if sender.is_some_and(|s| s == State::Dead) || sender.is_some() && !known {
            let all: Vec<Member> = std::iter::once(self.me.clone())
                .chain(
                    self.members
                        .values()
                        .filter(|m| m.state != State::Dead)
                        .cloned(),
                )
                .collect();
            for m in all {
                if !self.queue.iter().any(|(q, _)| q.id == m.id) {
                    self.queue.push_back((m, 0));
                }
            }
            if sender == Some(State::Dead) {
                let dead = self.members[&wire.from].clone();
                self.enqueue(dead);
            }
        }
        // Hearing from a member at all is evidence it is alive, but only its
        // own announcement (above) can change its state or incarnation.
        let mut out = Vec::new();
        match wire.msg {
            Msg::Ping { seq } => out.push(self.send(from, Msg::Ack { seq })),
            Msg::Ack { seq } => {
                if let Some(p) = self.probe.as_mut()
                    && p.seq == seq
                    && p.target == wire.from
                {
                    p.acked = true;
                }
                // An ack for a ping this node sent on another's behalf.
                let mut done = Vec::new();
                for (i, r) in self.relays.iter().enumerate() {
                    if r.target == wire.from {
                        done.push(i);
                    }
                }
                for i in done.into_iter().rev() {
                    let r = self.relays.remove(i);
                    let msg = Msg::IndirectAck {
                        seq: r.requester_seq,
                        target: r.target,
                    };
                    out.push(self.send(r.requester, msg));
                }
            }
            Msg::PingReq { seq, target } => {
                if let Some(t) = self.members.get(&target).map(|m| m.addr) {
                    let my_seq = self.next_seq();
                    self.relays.push(Relay {
                        requester: from,
                        requester_seq: seq,
                        target,
                        expires: now + self.settings.period.as_millis() as u64,
                    });
                    out.push(self.send(t, Msg::Ping { seq: my_seq }));
                }
            }
            Msg::IndirectAck { seq, target } => {
                if let Some(p) = self.probe.as_mut()
                    && p.seq == seq
                    && p.target == target
                {
                    p.acked = true;
                }
            }
        }
        Ok(out)
    }

    /// Leaves the cluster: announces this node dead so peers stop at once
    /// rather than after a suspicion timeout.
    pub fn leave(&mut self) -> Vec<Outgoing> {
        self.me.state = State::Dead;
        let me = self.me.clone();
        self.enqueue(me);
        let targets: Vec<SocketAddr> = self
            .members
            .values()
            .filter(|m| m.state != State::Dead)
            .map(|m| m.addr)
            .collect();
        let mut out = Vec::new();
        for t in targets {
            let seq = self.next_seq();
            out.push(self.send(t, Msg::Ping { seq }));
        }
        out
    }
}

/// A running membership: the live members as they change.
pub struct Membership {
    pub members: watch::Receiver<Vec<Member>>,
    task: Option<JoinHandle<()>>,
    stop: Arc<tokio::sync::Notify>,
}

impl Membership {
    /// Leaves the cluster and stops.
    pub async fn leave(mut self) {
        self.stop.notify_one();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Membership {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// Runs `swim` on `socket` until [`Membership::leave`]: a tick every tenth
/// of the ping timeout, each datagram handled as it arrives.
pub fn run(mut swim: Swim, socket: UdpSocket) -> Membership {
    let (tx, members) = watch::channel(swim.alive());
    let stop = Arc::new(tokio::sync::Notify::new());
    let stopped = stop.clone();
    let start = tokio::time::Instant::now();
    let step = (swim.settings.ping_timeout / 10).max(Duration::from_millis(5));
    let task = tokio::spawn(async move {
        let now = || start.elapsed().as_millis() as u64;
        let mut buf = vec![0u8; MAX_DATAGRAM + 1];
        let mut ticker = tokio::time::interval(step);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let out = tokio::select! {
                _ = stopped.notified() => {
                    for o in swim.leave() {
                        let _ = socket.send_to(&o.bytes, o.to).await;
                    }
                    return;
                }
                _ = ticker.tick() => swim.tick(now()),
                got = socket.recv_from(&mut buf) => match got {
                    Ok((n, from)) => match swim.receive(now(), from, &buf[..n]) {
                        Ok(out) => out,
                        Err(why) => {
                            tracing::debug!(%from, ?why, "swim datagram refused");
                            Vec::new()
                        }
                    },
                    Err(_) => Vec::new(),
                },
            };
            for o in out {
                let _ = socket.send_to(&o.bytes, o.to).await;
            }
            let alive = swim.alive();
            tx.send_if_modified(|current| {
                if *current == alive {
                    false
                } else {
                    *current = alive;
                    true
                }
            });
        }
    });
    Membership {
        members,
        task: Some(task),
        stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"a cluster key of at least 32 bytes!!";

    fn addr(i: usize) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, i as u8 + 1], 7946))
    }

    fn settings() -> Settings {
        Settings::default()
    }

    /// A simulated network of `n` nodes on a millisecond clock, dropping
    /// each datagram with probability `loss`, delivering the rest after
    /// 1 ms.
    struct Net {
        nodes: Vec<Option<Swim>>,
        in_flight: Vec<(u64, SocketAddr, SocketAddr, Vec<u8>)>,
        now: u64,
        loss: f64,
        rng: u64,
        sent: u64,
        dropped: u64,
    }

    impl Net {
        fn new(n: usize, loss: f64, seed: u64) -> Net {
            let nodes = (0..n)
                .map(|i| {
                    let mut s = Swim::new(
                        &format!("n{i}"),
                        addr(i),
                        "",
                        KEY,
                        settings(),
                        seed + i as u64,
                    );
                    s.join(&[addr(0)]);
                    Some(s)
                })
                .collect();
            Net {
                nodes,
                in_flight: Vec::new(),
                now: 0,
                loss,
                rng: seed,
                sent: 0,
                dropped: 0,
            }
        }

        fn chance(&mut self) -> f64 {
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            (self.rng >> 11) as f64 / (1u64 << 53) as f64
        }

        fn post(&mut self, from: SocketAddr, out: Vec<Outgoing>) {
            for o in out {
                self.sent += 1;
                if self.chance() < self.loss {
                    self.dropped += 1;
                    continue;
                }
                self.in_flight.push((self.now + 1, from, o.to, o.bytes));
            }
        }

        fn step(&mut self) {
            self.now += 5;
            let now = self.now;
            let (due, later): (Vec<_>, Vec<_>) =
                self.in_flight.drain(..).partition(|(t, ..)| *t <= now);
            self.in_flight = later;
            for (_, from, to, bytes) in due {
                let i = (to
                    .ip()
                    .to_string()
                    .rsplit('.')
                    .next()
                    .unwrap()
                    .parse::<usize>()
                    .unwrap())
                    - 1;
                if let Some(node) = self.nodes[i].as_mut() {
                    let out = node.receive(now, from, &bytes).unwrap();
                    let me = node.me().addr;
                    self.post(me, out);
                }
            }
            for i in 0..self.nodes.len() {
                if let Some(node) = self.nodes[i].as_mut() {
                    let out = node.tick(now);
                    let me = node.me().addr;
                    self.post(me, out);
                }
            }
        }

        fn run_ms(&mut self, ms: u64) {
            let until = self.now + ms;
            while self.now < until {
                self.step();
            }
        }

        fn view(&self, i: usize) -> Vec<(String, State)> {
            let node = self.nodes[i].as_ref().unwrap();
            node.members().map(|m| (m.id.clone(), m.state)).collect()
        }

        fn alive_ids(&self, i: usize) -> Vec<String> {
            let mut ids: Vec<String> = self.nodes[i]
                .as_ref()
                .unwrap()
                .alive()
                .into_iter()
                .map(|m| m.id)
                .collect();
            ids.sort();
            ids
        }

        fn converged(&self, want: &[String]) -> bool {
            self.nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.is_some())
                .all(|(i, _)| self.alive_ids(i) == want)
        }
    }

    fn ids(range: impl Iterator<Item = usize>) -> Vec<String> {
        let mut v: Vec<String> = range.map(|i| format!("n{i}")).collect();
        v.sort();
        v
    }

    #[test]
    fn ten_nodes_join_through_one_seed() {
        let mut net = Net::new(10, 0.0, 1);
        net.run_ms(5_000);
        assert!(net.converged(&ids(0..10)), "{:?}", net.view(3));
    }

    #[test]
    fn a_killed_node_is_declared_dead_everywhere_within_three_seconds() {
        let mut slowest = 0;
        for (loss, seed) in [0.0, 0.05, 0.1]
            .into_iter()
            .flat_map(|loss| (0..20).map(move |s| (loss, 100 + s)))
        {
            let mut net = Net::new(10, loss, seed);
            net.run_ms(10_000);
            assert!(net.converged(&ids(0..10)), "loss {loss}: {:?}", net.view(1));
            net.nodes[7] = None;
            let killed = net.now;
            let rest: Vec<String> = ids((0..10).filter(|i| *i != 7));
            while !net.converged(&rest) {
                net.step();
                assert!(
                    net.now - killed <= 3_000,
                    "loss {loss}: not detected in 3 s: {:?}",
                    net.view(1)
                );
            }
            slowest = slowest.max(net.now - killed);
            // And nobody else was declared dead on the way.
            net.run_ms(5_000);
            assert!(net.converged(&rest), "loss {loss}: {:?}", net.view(2));
        }
        println!("slowest detection over 60 runs: {slowest} ms");
    }

    #[test]
    fn a_suspected_node_refutes_with_a_higher_incarnation() {
        let mut net = Net::new(3, 0.0, 5);
        net.run_ms(3_000);
        // n1 hears that n2 is suspect at its current incarnation.
        let n2 = net.nodes[2].as_ref().unwrap().me().clone();
        let rumour = Member {
            state: State::Suspect,
            ..n2.clone()
        };
        let node1 = net.nodes[1].as_mut().unwrap();
        node1.apply(rumour, net.now);
        assert_eq!(node1.members.get("n2").unwrap().state, State::Suspect);
        net.run_ms(1_000);
        // n2 learnt of it, raised its incarnation, and every node agrees.
        let me = net.nodes[2].as_ref().unwrap().me().clone();
        assert!(me.incarnation > n2.incarnation);
        for i in [0, 1] {
            let m = net.nodes[i]
                .as_ref()
                .unwrap()
                .members
                .get("n2")
                .unwrap()
                .clone();
            assert_eq!(
                (m.state, m.incarnation),
                (State::Alive, me.incarnation),
                "node {i}"
            );
        }
    }

    #[test]
    fn a_node_that_leaves_is_dead_at_once_and_a_restart_rejoins() {
        let mut net = Net::new(4, 0.0, 6);
        net.run_ms(3_000);
        let out = net.nodes[3].as_mut().unwrap().leave();
        net.post(addr(3), out);
        net.nodes[3] = None;
        net.run_ms(50);
        assert!(net.converged(&ids(0..3)));
        // A new process for n3: incarnation 0, so it must out-rank the old
        // Dead entry by refuting it when it hears of it.
        let mut again = Swim::new("n3", addr(3), "", KEY, settings(), 99);
        again.join(&[addr(0)]);
        net.nodes[3] = Some(again);
        net.run_ms(5_000);
        assert!(net.converged(&ids(0..4)), "{:?}", net.view(0));
    }

    #[test]
    fn the_precedence_rules() {
        let mut s = Swim::new("me", addr(0), "", KEY, settings(), 1);
        let m = |state, incarnation| Member {
            id: "x".into(),
            addr: addr(1),
            meta: String::new(),
            state,
            incarnation,
        };
        assert!(
            !s.apply(m(State::Dead, 0), 0),
            "an unknown dead member is ignored"
        );
        assert!(s.apply(m(State::Alive, 1), 0));
        assert!(
            !s.apply(m(State::Alive, 1), 0),
            "same incarnation, no change"
        );
        assert!(!s.apply(m(State::Alive, 0), 0), "older");
        assert!(
            s.apply(m(State::Suspect, 1), 0),
            "suspect at the same incarnation"
        );
        assert!(!s.apply(m(State::Suspect, 1), 0));
        assert!(s.apply(m(State::Alive, 2), 0), "refuted");
        assert!(s.apply(m(State::Dead, 2), 0));
        assert!(!s.apply(m(State::Suspect, 2), 0), "dead beats suspect");
        assert!(s.apply(m(State::Suspect, 3), 0), "unless newer");
        assert!(s.apply(m(State::Dead, 3), 0));
        assert!(
            s.apply(m(State::Alive, 4), 0),
            "a restart with a higher incarnation"
        );
        assert!(!s.apply(m(State::Dead, 3), 0));
        // About itself: a suspicion is refuted, an alive is ignored.
        let me = |state, incarnation| Member {
            id: "me".into(),
            addr: addr(0),
            meta: String::new(),
            state,
            incarnation,
        };
        assert!(!s.apply(me(State::Alive, 9), 0));
        assert_eq!(s.me().incarnation, 0);
        s.apply(me(State::Suspect, 5), 0);
        assert_eq!(s.me().incarnation, 6);
        s.apply(me(State::Dead, 2), 0);
        assert_eq!(s.me().incarnation, 6, "an older rumour needs no refuting");
    }

    #[test]
    fn datagrams_are_signed_and_bounded() {
        let mut a = Swim::new("a", addr(0), "", KEY, settings(), 1);
        let mut b = Swim::new("b", addr(1), "", KEY, settings(), 2);
        let mut c = Swim::new(
            "c",
            addr(2),
            "",
            b"another cluster's key, 32 bytes or so",
            settings(),
            3,
        );
        b.join(&[addr(0)]);
        let ping = b.tick(0);
        assert_eq!(ping.len(), 1);
        assert_eq!(
            c.receive(0, addr(1), &ping[0].bytes),
            Err(Refused::BadSignature)
        );
        let mut flipped = ping[0].bytes.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert_eq!(a.receive(0, addr(1), &flipped), Err(Refused::BadSignature));
        assert_eq!(a.receive(0, addr(1), &[0; 10]), Err(Refused::TooShort));
        assert_eq!(
            a.receive(0, addr(1), &[0; MAX_DATAGRAM + 1]),
            Err(Refused::TooLarge)
        );
        // Signed but not a message.
        let body = b"{\"not\":\"swim\"}";
        let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, KEY), body);
        let forged = [tag.as_ref(), body.as_slice()].concat();
        assert_eq!(a.receive(0, addr(1), &forged), Err(Refused::Malformed));
        let ack = a.receive(0, addr(1), &ping[0].bytes).unwrap();
        assert_eq!(ack.len(), 1);
        assert_eq!(ack[0].to, addr(1));
        assert!(a.members().any(|m| m.id == "b"));
        // Many changes still fit one datagram: the rest wait their turn.
        for i in 0..200 {
            a.apply(
                Member {
                    id: format!("member-with-a-long-name-{i:04}"),
                    addr: addr(3),
                    meta: "{\"http\":\"10.0.0.4:8080\",\"weight\":1}".into(),
                    state: State::Alive,
                    incarnation: 1,
                },
                0,
            );
        }
        let out = a.tick(1_000);
        assert!(out.iter().all(|o| o.bytes.len() <= MAX_DATAGRAM));
        assert!(!a.queue.is_empty());
    }

    #[tokio::test]
    async fn three_nodes_over_udp_find_each_other_and_notice_a_departure() {
        let fast = Settings {
            period: Duration::from_millis(100),
            ping_timeout: Duration::from_millis(30),
            suspicion: Duration::from_millis(300),
            ..Settings::default()
        };
        let mut sockets = Vec::new();
        for _ in 0..3 {
            sockets.push(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        }
        let addrs: Vec<SocketAddr> = sockets.iter().map(|s| s.local_addr().unwrap()).collect();
        let mut nodes = Vec::new();
        for (i, socket) in sockets.into_iter().enumerate() {
            let mut s = Swim::new(
                &format!("u{i}"),
                addrs[i],
                &format!("meta {i}"),
                KEY,
                fast.clone(),
                i as u64,
            );
            s.join(&[addrs[0]]);
            nodes.push(run(s, socket));
        }
        let wait = |rx: &watch::Receiver<Vec<Member>>, n: usize| {
            let mut rx = rx.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(5), rx.wait_for(|m| m.len() == n))
                    .await
                    .expect("membership did not settle")
                    .unwrap()
                    .clone()
            }
        };
        let seen = wait(&nodes[2].members, 3).await;
        assert!(seen.iter().any(|m| m.id == "u1" && m.meta == "meta 1"));
        let gone = nodes.remove(1);
        gone.leave().await;
        let after = wait(&nodes[0].members, 2).await;
        assert!(after.iter().all(|m| m.id != "u1"));
    }
}
