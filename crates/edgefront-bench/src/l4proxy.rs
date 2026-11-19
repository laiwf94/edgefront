//! The L4 proxy's cost: libpacer's `bench/bench_proxy.c` as a scenario,
//! run against the upstream directly, through legacy pacerd and through
//! `edgefront limits`, each proxy started on the same config with no
//! limits set:
//!
//! - **bulk**: one connection downloads `bulk_bytes` of zeros (pacerd with
//!   one worker and 64 KiB buffers), in MiB/s;
//! - **latency**: one connection sends 64 bytes and waits for the echo,
//!   `round_trips` times (four workers), p50 and p99 in microseconds;
//! - **concurrent**: `concurrent_clients` connections each do
//!   `concurrent_trips` round trips at once, in round trips per second,
//!   with the proxy's CPU time per round trip where the host has /proc.
//!
//! Each target's point is the median of `runs` runs. The upstreams are
//! threads in this process, as in the C, so the proxy is the only other
//! process.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// An L4 scenario, as read from TOML.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    pub bulk_bytes: u64,
    pub round_trips: usize,
    pub concurrent_clients: usize,
    pub concurrent_trips: usize,
    pub runs: usize,
}

impl Scenario {
    pub fn parse(text: &str) -> Result<Self, String> {
        let s: Scenario = toml::from_str(text).map_err(|e| e.to_string().trim_end().to_string())?;
        if s.bulk_bytes == 0
            || s.round_trips == 0
            || s.concurrent_clients == 0
            || s.concurrent_trips == 0
            || s.runs == 0
        {
            return Err("every count must be at least 1".into());
        }
        Ok(s)
    }
}

/// Where the clients connect.
#[derive(Clone, Debug)]
pub enum Target {
    Direct,
    /// `pacerd -c CONFIG`.
    Pacerd(PathBuf),
    /// `edgefront limits -c CONFIG`.
    Edgefront(PathBuf),
}

impl Target {
    pub fn name(&self) -> &'static str {
        match self {
            Target::Direct => "direct",
            Target::Pacerd(_) => "pacerd",
            Target::Edgefront(_) => "edgefront",
        }
    }
}

/// One target's numbers.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Point {
    pub target: String,
    pub bulk_mib_s: f64,
    pub p50_us: f64,
    pub p99_us: f64,
    pub trips_per_s: f64,
    /// The proxy's CPU time per concurrent round trip, where measurable.
    pub cpu_us_per_trip: Option<f64>,
}

/// An upstream thread: zeros to every connection, or an echo.
struct Upstream {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl Upstream {
    fn start(zeros: Option<u64>) -> std::io::Result<Self> {
        let l = TcpListener::bind("127.0.0.1:0")?;
        let addr = l.local_addr()?;
        l.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !s2.load(Ordering::Relaxed) {
                match l.accept() {
                    Ok((mut s, _)) => {
                        let _ = s.set_nonblocking(false);
                        let _ = s.set_nodelay(true);
                        std::thread::spawn(move || {
                            let mut buf = vec![0u8; 256 * 1024];
                            match zeros {
                                Some(total) => {
                                    let mut left = total;
                                    while left > 0 {
                                        let n = left.min(buf.len() as u64) as usize;
                                        if s.write_all(&buf[..n]).is_err() {
                                            return;
                                        }
                                        left -= n as u64;
                                    }
                                }
                                None => loop {
                                    match s.read(&mut buf) {
                                        Ok(0) | Err(_) => return,
                                        Ok(n) => {
                                            if s.write_all(&buf[..n]).is_err() {
                                                return;
                                            }
                                        }
                                    }
                                },
                            }
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(1)),
                }
            }
        });
        Ok(Self { addr, stop })
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A proxy process in front of one upstream.
struct Proxy {
    child: Child,
    addr: SocketAddr,
    dir: PathBuf,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status();
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end && matches!(self.child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn free_port() -> std::io::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// Starts `target`'s proxy with `workers` workers in front of `upstream`,
/// with pacer's benchmark config: 64 KiB buffers, room for 10,000
/// connections, no limits.
fn start(
    target: &Target,
    upstream: SocketAddr,
    workers: u32,
    work: &Path,
) -> Result<Proxy, String> {
    let (bin, sub): (&Path, &[&str]) = match target {
        Target::Direct => return Err("direct has no proxy".into()),
        Target::Pacerd(p) => (p, &[]),
        Target::Edgefront(p) => (p, &["limits"]),
    };
    let port = free_port().map_err(|e| e.to_string())?;
    let dir = work.join(format!("{}-{port}", target.name()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let conf = dir.join("bench.conf");
    std::fs::write(
        &conf,
        format!(
            "workers {workers}\nlistener bench {{\n listen 127.0.0.1:{port}\n upstream {upstream}\n buffer 64k\n max_conns 10000\n}}\n"
        ),
    )
    .map_err(|e| e.to_string())?;
    let log = dir.join("log");
    let err = std::fs::File::create(&log).map_err(|e| e.to_string())?;
    let child = Command::new(bin)
        .args(sub)
        .arg("-c")
        .arg(&conf)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(err)
        .spawn()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    let p = Proxy {
        child,
        addr: SocketAddr::from(([127, 0, 0, 1], port)),
        dir,
    };
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if std::fs::read_to_string(&log).is_ok_and(|t| t.contains("msg=ready")) {
            return Ok(p);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(format!("{} did not start", target.name()))
}

fn connect(addr: SocketAddr) -> Result<TcpStream, String> {
    let s = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    let _ = s.set_nodelay(true);
    Ok(s)
}

/// MiB/s for one download of `total` bytes.
fn bulk(addr: SocketAddr, total: u64) -> Result<f64, String> {
    let mut s = connect(addr)?;
    let start = Instant::now();
    let mut buf = vec![0u8; 256 * 1024];
    let mut got = 0u64;
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got += n as u64,
            Err(e) => return Err(e.to_string()),
        }
    }
    if got != total {
        return Err(format!("bulk transfer short: {got} of {total}"));
    }
    Ok(total as f64 / start.elapsed().as_secs_f64() / f64::from(1 << 20))
}

/// One 64-byte round trip on `s`.
fn trip(s: &mut TcpStream, buf: &mut [u8; 64]) -> Result<(), String> {
    s.write_all(buf).map_err(|e| e.to_string())?;
    s.read_exact(buf).map_err(|e| e.to_string())
}

/// p50 and p99 of `n` round trips, in microseconds.
fn latency(addr: SocketAddr, n: usize) -> Result<(f64, f64), String> {
    let mut s = connect(addr)?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [b'r'; 64];
    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        trip(&mut s, &mut buf)?;
        samples.push(t.elapsed().as_nanos() as u64);
    }
    samples.sort_unstable();
    let at = |q: usize| samples[(n * q / 100).min(n - 1)] as f64 / 1000.0;
    Ok((at(50), at(99)))
}

/// Round trips per second over `clients` connections of `trips` each.
fn concurrent(addr: SocketAddr, clients: usize, trips: usize) -> Result<f64, String> {
    let start = Instant::now();
    let hs: Vec<_> = (0..clients)
        .map(|_| {
            std::thread::spawn(move || -> Result<(), String> {
                let mut s = connect(addr)?;
                let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = [b'c'; 64];
                for _ in 0..trips {
                    trip(&mut s, &mut buf)?;
                }
                Ok(())
            })
        })
        .collect();
    for h in hs {
        h.join().map_err(|_| "a client panicked")??;
    }
    Ok((clients * trips) as f64 / start.elapsed().as_secs_f64())
}

/// A process's CPU seconds from /proc, or `None` without it.
fn cpu_secs(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split(' ').collect();
    let ticks = f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?;
    Some(ticks / 100.0)
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// One run against one target.
fn run_once(s: &Scenario, target: &Target, work: &Path) -> Result<Point, String> {
    let zeros = Upstream::start(Some(s.bulk_bytes)).map_err(|e| e.to_string())?;
    let echo = Upstream::start(None).map_err(|e| e.to_string())?;
    let (px_bulk, px_echo) = match target {
        Target::Direct => (None, None),
        t => (
            Some(start(t, zeros.addr, 1, work)?),
            Some(start(t, echo.addr, 4, work)?),
        ),
    };
    let bulk_addr = px_bulk.as_ref().map_or(zeros.addr, |p| p.addr);
    let echo_addr = px_echo.as_ref().map_or(echo.addr, |p| p.addr);
    let bulk_mib_s = bulk(bulk_addr, s.bulk_bytes)?;
    let (p50_us, p99_us) = latency(echo_addr, s.round_trips)?;
    let pid = px_echo.as_ref().map(|p| p.child.id());
    let cpu0 = pid.and_then(cpu_secs);
    let trips_per_s = concurrent(echo_addr, s.concurrent_clients, s.concurrent_trips)?;
    let cpu_us_per_trip = match (cpu0, pid.and_then(cpu_secs)) {
        (Some(a), Some(b)) => {
            Some((b - a) / (s.concurrent_clients * s.concurrent_trips) as f64 * 1e6)
        }
        _ => None,
    };
    Ok(Point {
        target: target.name().into(),
        bulk_mib_s,
        p50_us,
        p99_us,
        trips_per_s,
        cpu_us_per_trip,
    })
}

/// Each target's median point over the scenario's runs. The targets take
/// turns run by run, so drift on a shared machine spreads over all.
pub fn run(s: &Scenario, targets: &[Target], work: &Path) -> Result<Vec<Point>, String> {
    let mut all: Vec<Vec<Point>> = vec![Vec::new(); targets.len()];
    for _ in 0..s.runs {
        for (i, t) in targets.iter().enumerate() {
            all[i].push(run_once(s, t, work)?);
        }
    }
    Ok(all
        .into_iter()
        .zip(targets)
        .map(|(pts, t)| Point {
            target: t.name().into(),
            bulk_mib_s: median(pts.iter().map(|p| p.bulk_mib_s).collect()),
            p50_us: median(pts.iter().map(|p| p.p50_us).collect()),
            p99_us: median(pts.iter().map(|p| p.p99_us).collect()),
            trips_per_s: median(pts.iter().map(|p| p.trips_per_s).collect()),
            cpu_us_per_trip: pts
                .iter()
                .map(|p| p.cpu_us_per_trip)
                .collect::<Option<Vec<f64>>>()
                .map(median),
        })
        .collect())
}

/// The Markdown table: one column per target.
pub fn markdown(points: &[Point]) -> String {
    let mut out = String::from("| |");
    for p in points {
        out.push_str(&format!(" {} |", p.target));
    }
    out.push_str("\n|---|");
    out.push_str(&"---:|".repeat(points.len()));
    out.push('\n');
    let row = |name: &str, f: &dyn Fn(&Point) -> String| {
        let mut r = format!("| {name} |");
        for p in points {
            r.push_str(&format!(" {} |", f(p)));
        }
        r.push('\n');
        r
    };
    out.push_str(&row("bulk download, one connection (MiB/s)", &|p| {
        format!("{:.0}", p.bulk_mib_s)
    }));
    out.push_str(&row("64-byte round trip, p50 (µs)", &|p| {
        format!("{:.1}", p.p50_us)
    }));
    out.push_str(&row("64-byte round trip, p99 (µs)", &|p| {
        format!("{:.1}", p.p99_us)
    }));
    out.push_str(&row("round trips/s, 32 connections", &|p| {
        format!("{:.0}", p.trips_per_s)
    }));
    out.push_str(&row("proxy CPU per round trip (µs)", &|p| {
        p.cpu_us_per_trip
            .map_or_else(|| "-".into(), |v| format!("{v:.1}"))
    }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scenario_parses_and_zero_counts_are_refused() {
        let text = "name = \"l4\"\nbulk_bytes = 1048576\nround_trips = 100\nconcurrent_clients = 4\nconcurrent_trips = 50\nruns = 1\n";
        let s = Scenario::parse(text).unwrap();
        assert_eq!(s.round_trips, 100);
        assert!(Scenario::parse(&text.replace("runs = 1", "runs = 0")).is_err());
        assert!(Scenario::parse(&format!("{text}x = 1\n")).is_err());
    }

    #[test]
    fn the_direct_target_measures_every_row() {
        let s = Scenario {
            name: "t".into(),
            bulk_bytes: 8 << 20,
            round_trips: 200,
            concurrent_clients: 4,
            concurrent_trips: 50,
            runs: 2,
        };
        let work = std::env::temp_dir().join(format!("l4bench-{}", std::process::id()));
        let pts = run(&s, &[Target::Direct], &work).unwrap();
        let p = &pts[0];
        assert_eq!(p.target, "direct");
        assert!(p.bulk_mib_s > 0.0 && p.p50_us > 0.0 && p.p99_us >= p.p50_us);
        assert!(p.trips_per_s > 0.0);
        assert_eq!(p.cpu_us_per_trip, None);
        let md = markdown(&pts);
        assert!(md.starts_with("| | direct |\n|---|---:|\n"), "{md}");
        assert!(md.contains("| proxy CPU per round trip (µs) | - |"), "{md}");
        assert!(start(&Target::Direct, p_addr(), 1, &work).is_err());
        assert_eq!(Target::Pacerd(PathBuf::new()).name(), "pacerd");
        assert_eq!(Target::Edgefront(PathBuf::new()).name(), "edgefront");
        assert_eq!(median(vec![2.0, 1.0]), 1.5);
    }

    fn p_addr() -> SocketAddr {
        "127.0.0.1:1".parse().unwrap()
    }
}
