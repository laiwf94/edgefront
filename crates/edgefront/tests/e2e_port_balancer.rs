//! End to end: the proxy balances real HTTP/1.1 traffic over three priority
//! levels the way steer did, pick for pick.
//!
//! Nine origins listen on fixed loopback addresses (`127.0.1.1:18601` to
//! `127.0.1.9:18609`; Maglev hashes the address, so it has to be the same
//! on every run) and answer each request with their index. One cluster
//! lists them interleaved across priorities 0, 1 and 2 with mixed weights
//! and steer's default panic threshold of 50%. For each policy (round
//! robin, least loaded, and Maglev on an `X-User` header) a proxy built
//! with a manual clock and a seeded SplitMix64 serves 20,000 requests, sent
//! one at a time on a kept-alive connection. Between requests a seeded
//! script walks the cluster through ten phases of 2,000 requests: all
//! healthy, the preferred level losing endpoints until traffic spills to
//! priority 1 and then 2, panic, ejections that run out as the clock moves,
//! a level-2-only phase and recovery. On top of the phases it flips health
//! at random, moves the clock 1 ms per request, feeds latency samples and
//! holds requests in flight on random endpoints, all through the state the
//! proxy's balancer reads ([`Proxy::endpoints`]).
//!
//! Every event and every request becomes a line of a `picks` script for
//! `legacy/steer/cmd/diffdrive`: what the test did to the endpoints, `r KEY
//! 1` for each request and `o I 0` for the latency the proxy observed on
//! the endpoint that answered (zero, since the manual clock does not move
//! during a request). Each run checks that the origins counted exactly the
//! requests the responses name, that every level and panic mode took
//! traffic, and that the per-endpoint counts and a digest of the whole pick
//! sequence equal `tests/fixtures/e2e_port_balancer.txt`, recorded from
//! steer. With `STEER_DIFFDRIVE` pointing at a build of diffdrive (`make
//! e2e-steer`, run in CI by the legacy-steer workflow), steer's balancer
//! answers the script live: the first request where its pick differs from
//! the proxy's fails the test, and its counts must equal the file too.
//! `E2E_BLESS=1` rewrites the file from steer.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use edgefront_core::clock::{Clock, ManualClock};
use edgefront_core::config::Config;
use edgefront_core::listener::{ServerLimits, serve};
use edgefront_proxy::balancer::maglev::hash_key;
use edgefront_proxy::balancer::rng::{Rng, SplitMix64};
use edgefront_proxy::server::Proxy;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/e2e_port_balancer.txt"
);

/// Each endpoint's `(priority, weight)`, in configuration order: levels
/// 0 = {0, 3, 5, 8}, 1 = {1, 4, 7} and 2 = {2, 6}.
const ENDPOINTS: [(u8, u32); 9] = [
    (0, 1),
    (1, 2),
    (2, 1),
    (0, 3),
    (1, 1),
    (0, 2),
    (2, 3),
    (1, 1),
    (0, 1),
];
const LEVELS: usize = 3;
const REQUESTS: usize = 20_000;
const PHASE: usize = 2_000;
const PANIC_THRESHOLD: u8 = 50;
/// The seed of the balancer's generator, shared with diffdrive.
const BALANCER_SEED: u64 = 20_261_109;
/// The seed of the script's own events.
const SCRIPT_SEED: u64 = 0x5eed_0002;
const POLICIES: [&str; 3] = ["round_robin", "least_loaded", "maglev"];

/// The endpoints marked unhealthy for each phase of 2,000 requests.
const DOWN: [&[usize]; REQUESTS / PHASE] = [
    // All healthy: level 0 takes everything.
    &[],
    // 3 of 4 at level 0: 105% after overprovisioning, still all of it.
    &[0],
    // Half of level 0: 70% there, the rest spills to level 1.
    &[0, 3],
    // One left at level 0: 35%, level 1 takes 65%.
    &[0, 3, 5],
    // Level 0 gone, 2 of 3 at level 1: 93%, level 2 takes the rest.
    &[0, 3, 5, 8, 1],
    // 1 of 3 at level 1 and 1 of 2 at level 2: 46% and 54%.
    &[0, 3, 5, 8, 1, 4, 2],
    // Only one level-0 endpoint: 35% in all, below 50%, so panic.
    &[0, 5, 8, 1, 4, 7, 2, 6],
    // All healthy, with ejections that run out (see `ejections`).
    &[],
    // Only level 2 is healthy, and it loses an endpoint to an ejection.
    &[0, 3, 5, 8, 1, 4, 7],
    // Recovery.
    &[],
];

fn address(i: usize) -> SocketAddr {
    SocketAddr::from(([127, 0, 1, i as u8 + 1], 18_601 + i as u16))
}

/// The ejections at the start of a phase: `(endpoint, base, max)`.
fn ejections(phase: usize) -> &'static [(usize, Duration, Duration)] {
    const S: Duration = Duration::from_secs(1);
    const PHASE_7: [(usize, Duration, Duration); 2] = [
        (0, Duration::from_secs(2), Duration::from_secs(10)),
        (3, S, S),
    ];
    const PHASE_8: [(usize, Duration, Duration); 1] = [(2, Duration::from_millis(500), S)];
    match phase {
        7 => &PHASE_7,
        8 => &PHASE_8,
        _ => &[],
    }
}

// ---------------------------------------------------------------------------
// The origins.

/// Starts the nine origins. Each answers every request on a connection with
/// its index and counts what it answered.
fn origins() -> Vec<Arc<AtomicU64>> {
    (0..ENDPOINTS.len())
        .map(|i| {
            let listener = TcpListener::bind(address(i)).unwrap_or_else(|e| {
                panic!(
                    "cannot bind origin {i} at {}: {e} (the test needs 127.0.1.0/24 on loopback)",
                    address(i)
                )
            });
            let count = Arc::new(AtomicU64::new(0));
            let seen = Arc::clone(&count);
            thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(conn) = conn else { return };
                    let seen = Arc::clone(&seen);
                    thread::spawn(move || answer(conn, i, &seen));
                }
            });
            count
        })
        .collect()
}

fn answer(conn: TcpStream, index: usize, seen: &AtomicU64) {
    let mut w = conn.try_clone().expect("a socket");
    let mut r = BufReader::new(conn);
    let body = index.to_string();
    loop {
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            if line == "\r\n" {
                break;
            }
        }
        seen.fetch_add(1, Ordering::Relaxed);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        if w.write_all(resp.as_bytes()).is_err() {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// The client.

/// Sends one GET on a kept-alive connection and returns the status and body.
fn get(conn: &mut BufReader<TcpStream>, user: Option<&str>) -> (u16, String) {
    let mut req = String::from("GET /e2e HTTP/1.1\r\nHost: e2e.example\r\n");
    if let Some(u) = user {
        let _ = write!(req, "X-User: {u}\r\n");
    }
    req.push_str("\r\n");
    conn.get_mut().write_all(req.as_bytes()).expect("a request");
    let mut line = String::new();
    conn.read_line(&mut line).expect("a status line");
    let status: u16 = line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("a status line, got {line:?}"));
    let mut length = 0;
    loop {
        line.clear();
        conn.read_line(&mut line).expect("a header");
        if line == "\r\n" {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().expect("a length");
        }
    }
    let mut body = vec![0; length];
    conn.read_exact(&mut body).expect("a body");
    (status, String::from_utf8(body).expect("a text body"))
}

// ---------------------------------------------------------------------------
// One run.

/// What one policy's run produced.
struct Run {
    /// One line per script operation, as diffdrive reads them.
    script: Vec<String>,
    /// The proxy's pick for each request, in the form diffdrive answers
    /// `r KEY 1` with: the endpoint index or -1.
    picks: Vec<String>,
}

fn config(policy: &str, listen: SocketAddr) -> Config {
    let eps: Vec<String> = ENDPOINTS
        .iter()
        .enumerate()
        .map(|(i, &(p, w))| {
            format!(
                "{{\"address\":\"{}\",\"weight\":{w},\"priority\":{p}}}",
                address(i)
            )
        })
        .collect();
    let hash = if policy == "maglev" {
        ",\"hash_header\":\"x-user\""
    } else {
        ""
    };
    let json = format!(
        "{{\"listen\":\"{listen}\",\"routes\":[{{\"host\":\"*\",\"path_prefix\":\"/\",\"cluster\":\"web\"}}],\
         \"clusters\":[{{\"name\":\"web\",\"policy\":\"{policy}\"{hash},\"panic_threshold_percent\":{PANIC_THRESHOLD},\
         \"timeout\":\"10s\",\"endpoints\":[{}]}}]}}",
        eps.join(",")
    );
    Config::from_json(json.as_bytes()).expect("a valid config")
}

/// Waits for `ep`'s in-flight count to come back to `want` once its last
/// response body has been dropped, so the next pick reads what steer's
/// would.
fn settle(proxy: &Proxy, ep: usize, want: i64) {
    let state = &proxy.endpoints("web").expect("the cluster")[ep].state;
    let until = Instant::now() + Duration::from_secs(10);
    while state.in_flight() != want {
        assert!(Instant::now() < until, "endpoint {ep} never settled");
        thread::yield_now();
    }
}

fn run(policy: &str, rt: &tokio::runtime::Runtime) -> Run {
    let clock = Arc::new(ManualClock::new());
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = config(policy, addr);
    let proxy = {
        let _in_runtime = rt.enter();
        Arc::new(Proxy::with_sources(
            &cfg,
            Arc::clone(&clock) as Arc<dyn Clock>,
            Arc::new(SplitMix64::new(BALANCER_SEED)),
        ))
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = rt.spawn(serve(
        listener,
        Arc::clone(&proxy),
        ServerLimits::default(),
        async move {
            let _ = stopped.await;
        },
    ));

    let eps = proxy.endpoints("web").expect("the cluster");
    let rng = SplitMix64::new(SCRIPT_SEED);
    let mut script = Vec::with_capacity(REQUESTS * 4);
    let mut picks = Vec::with_capacity(REQUESTS);
    let mut healthy = [true; ENDPOINTS.len()];
    // Requests the script holds in flight: (endpoint, request to release at).
    let mut held: Vec<(usize, usize)> = Vec::new();
    let mut extra = [0i64; ENDPOINTS.len()];
    let set_health = |script: &mut Vec<String>, i: usize, h: bool| {
        eps[i].state.set_healthy(h);
        script.push(format!("h {i} {}", u8::from(h)));
    };
    let mut conn = BufReader::new(TcpStream::connect(addr).expect("the proxy"));

    for n in 0..REQUESTS {
        if n % PHASE == 0 {
            let phase = n / PHASE;
            for (i, h) in healthy.iter_mut().enumerate() {
                let want = !DOWN[phase].contains(&i);
                if *h != want {
                    *h = want;
                    set_health(&mut script, i, want);
                }
            }
            for &(i, base, max) in ejections(phase) {
                eps[i].state.eject(clock.now(), base, max, "e2e");
                script.push(format!("e {i} {} {}", base.as_nanos(), max.as_nanos()));
            }
            if phase == REQUESTS / PHASE - 1 {
                for (i, ep) in eps.iter().enumerate() {
                    ep.state.clear_ejection();
                    script.push(format!("c {i}"));
                }
            }
        }
        clock.advance(Duration::from_millis(1));
        script.push("t 1000000".to_owned());
        if n % 211 == 105 {
            // A random flip on top of the phase, undone by the next phase.
            let i = rng.below(ENDPOINTS.len());
            healthy[i] = !healthy[i];
            set_health(&mut script, i, healthy[i]);
        }
        if n % 37 == 0 {
            let i = rng.below(ENDPOINTS.len());
            let sample = Duration::from_micros(500 + rng.below(50_000) as u64);
            eps[i].state.observe_latency(sample, clock.now());
            script.push(format!("o {i} {}", sample.as_nanos()));
        }
        if n % 97 == 0 {
            let i = rng.below(ENDPOINTS.len());
            eps[i].state.acquire();
            extra[i] += 1;
            held.push((i, n + 13 + rng.below(40)));
            script.push(format!("a {i}"));
        }
        while let Some(at) = held.iter().position(|&(_, until)| until == n) {
            let (i, _) = held.swap_remove(at);
            eps[i].state.release();
            extra[i] -= 1;
            script.push(format!("x {i}"));
        }

        let user =
            (policy == "maglev" && rng.below(10) != 0).then(|| format!("user-{}", rng.below(500)));
        let key = user
            .as_deref()
            .map_or_else(|| "-".to_owned(), |u| hash_key(u.as_bytes()).to_string());
        script.push(format!("r {key} 1"));
        let (status, body) = get(&mut conn, user.as_deref());
        match status {
            200 => {
                let i: usize = body.parse().expect("an origin's index");
                settle(&proxy, i, extra[i]);
                script.push(format!("o {i} 0"));
                picks.push(i.to_string());
            }
            503 => {
                assert!(body.contains("\"no_healthy_upstream\""), "{body}");
                picks.push("-1".to_owned());
            }
            s => panic!("request {n}: status {s}: {body}"),
        }
    }
    drop(conn);
    let _ = stop.send(());
    rt.block_on(server).expect("the server");
    Run { script, picks }
}

// ---------------------------------------------------------------------------
// Summaries and steer.

/// FNV-1a over the pick lines, each followed by a newline.
fn digest(picks: &[String]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in picks {
        for &b in p.as_bytes().iter().chain(b"\n") {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// The fixture's block for one policy's picks.
fn summary(policy: &str, picks: &[String]) -> String {
    let mut counts = [0u64; ENDPOINTS.len()];
    let mut levels = [0u64; LEVELS];
    let mut none = 0;
    for p in picks {
        match p.parse::<usize>() {
            Ok(i) => {
                counts[i] += 1;
                levels[usize::from(ENDPOINTS[i].0)] += 1;
            }
            Err(_) => none += 1,
        }
    }
    let join = |xs: &[u64]| xs.iter().map(u64::to_string).collect::<Vec<_>>().join(" ");
    format!(
        "policy {policy}\nrequests {}\ncounts {}\nlevels {}\nunavailable {none}\npicks {:016x}\n",
        picks.len(),
        join(&counts),
        join(&levels),
        digest(picks)
    )
}

/// Runs the script through steer's balancer and returns its pick for each
/// request.
fn steer_picks(diffdrive: &str, policy: &str, script: &[String]) -> Vec<String> {
    let endpoints: Vec<serde_json::Value> = ENDPOINTS
        .iter()
        .enumerate()
        .map(|(i, &(p, w))| {
            serde_json::json!({"address": address(i).to_string(), "weight": w, "priority": p})
        })
        .collect();
    let query = serde_json::json!({
        "op": "picks",
        "policy": policy,
        "panic_threshold": PANIC_THRESHOLD,
        "seed": BALANCER_SEED,
        "table_size": 65_537,
        "endpoints": endpoints,
        "script": script.join("\n"),
    });
    let mut child = Command::new(diffdrive)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("cannot run {diffdrive}: {e}"));
    {
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "{query}").unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "diffdrive failed: {:?}", out.status);
    let answer: serde_json::Value =
        serde_json::from_slice(out.stdout.split(|&c| c == b'\n').next().unwrap())
            .expect("a JSON answer");
    assert_eq!(answer["ok"], true, "steer refused the scenario: {answer}");
    let lines = answer["lines"].as_array().expect("lines");
    assert_eq!(lines.len(), script.len());
    script
        .iter()
        .zip(lines)
        .filter(|(op, _)| op.starts_with("r "))
        .map(|(_, l)| l.as_str().expect("a line").to_owned())
        .collect()
}

/// The fixture's blocks by policy.
fn fixture() -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(FIXTURE).expect("the fixture");
    let mut out = BTreeMap::new();
    let mut current: Option<(String, String)> = None;
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        if let Some(p) = line.strip_prefix("policy ") {
            out.extend(current.take());
            current = Some((p.to_owned(), String::new()));
        }
        if let Some((_, block)) = current.as_mut() {
            block.push_str(line);
            block.push('\n');
        }
    }
    out.extend(current);
    out
}

#[test]
fn balancing_over_three_priority_levels_matches_steer() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let counted = origins();
    let rt = Arc::new(rt);
    let runs: Vec<(String, Run)> = thread::scope(|s| {
        let handles: Vec<_> = POLICIES
            .iter()
            .map(|&p| {
                let rt = Arc::clone(&rt);
                s.spawn(move || (p.to_owned(), run(p, &rt)))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // The origins answered exactly the requests the responses name.
    let mut answered = [0u64; ENDPOINTS.len()];
    for (_, r) in &runs {
        for i in r.picks.iter().filter_map(|p| p.parse::<usize>().ok()) {
            answered[i] += 1;
        }
    }
    let seen: Vec<u64> = counted.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    assert_eq!(seen, answered, "origin counts against responses");

    let mut blocks = BTreeMap::new();
    for (policy, r) in &runs {
        assert_eq!(r.picks.len(), REQUESTS);
        // Every level took traffic, and panic sent some to endpoints
        // marked unhealthy: in phase 6 only endpoint 3 is healthy.
        let mut levels = [0; LEVELS];
        for i in r.picks.iter().filter_map(|p| p.parse::<usize>().ok()) {
            levels[usize::from(ENDPOINTS[i].0)] += 1;
        }
        assert!(levels.iter().all(|&l| l > 500), "{policy}: {levels:?}");
        let panicked = r.picks[6 * PHASE..7 * PHASE]
            .iter()
            .filter(|p| p.as_str() != "3" && p.as_str() != "-1")
            .count();
        assert!(panicked > 1_000, "{policy}: {panicked} picks in panic");
        blocks.insert(policy.clone(), summary(policy, &r.picks));
    }

    if let Ok(diffdrive) = std::env::var("STEER_DIFFDRIVE") {
        let mut steer_blocks = BTreeMap::new();
        for (policy, r) in &runs {
            let steer = steer_picks(&diffdrive, policy, &r.script);
            if let Some(n) = (0..REQUESTS).find(|&n| steer[n] != r.picks[n]) {
                panic!(
                    "{policy}: request {n} (phase {}): steer picked {} and edgefront {}",
                    n / PHASE,
                    steer[n],
                    r.picks[n]
                );
            }
            steer_blocks.insert(policy.clone(), summary(policy, &steer));
        }
        if std::env::var_os("E2E_BLESS").is_some() {
            let mut text = String::from(
                "# Recorded from steer's balancer through legacy/steer/cmd/diffdrive by\n\
                 # crates/edgefront/tests/e2e_port_balancer.rs (E2E_BLESS=1 make e2e-steer).\n\
                 # counts: requests per endpoint in configuration order; levels: per\n\
                 # priority; picks: FNV-1a of the pick sequence, one pick per line.\n",
            );
            for b in steer_blocks.values() {
                text.push_str(b);
            }
            std::fs::write(FIXTURE, text).expect("the fixture");
        }
        assert_eq!(steer_blocks, fixture(), "steer against the fixture");
    }
    assert_eq!(blocks, fixture(), "edgefront against the fixture");
}
