//! Starting a cache binary on a logical clock.
//!
//! Each node runs as a child process with libfaketime preloaded, reading
//! the time from a file the runner rewrites whenever the log's clock
//! moves. Wall-clock time is frozen between writes, so both nodes compute
//! every Age, freshness lifetime and `ttl=` from exactly the same instant,
//! however long a request takes in real time. Monotonic time is left
//! alone (`DONT_FAKE_MONOTONIC=1`), so tokio's timers, and with them the
//! nodes' timeouts and health checks, keep running on the real clock.

use std::fs;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The memory budget both nodes run with. The whole generated working set
/// fits in one of its shards, so nothing is evicted, whatever the nodes'
/// per-process hash seeds do to the shard each key lands in.
pub const MEMORY_BYTES: usize = 4 << 30;

/// Shards of that budget.
pub const SHARDS: usize = 16;

/// The object size limit both nodes run with; `/big/{i}` is over it.
pub const MAX_OBJECT_BYTES: usize = 64 << 10;

/// Where Debian and Ubuntu install the multithreaded libfaketime.
pub const DEFAULT_FAKETIME: &str = "/usr/lib/x86_64-linux-gnu/faketime/libfaketimeMT.so.1";

/// `secs` since the Unix epoch as `YYYY-MM-DD hh:mm:ss` in UTC, the
/// absolute (frozen) form libfaketime reads.
pub fn faketime_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// The file the nodes read the time from.
#[derive(Debug)]
pub struct Clock {
    path: PathBuf,
    now: u64,
}

impl Clock {
    /// Creates the file at `path`, set to `now`.
    pub fn create(path: PathBuf, now: u64) -> io::Result<Self> {
        let mut clock = Clock { path, now };
        clock.write(now)?;
        Ok(clock)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn now(&self) -> u64 {
        self.now
    }

    /// Moves the clock to `now`. The file is replaced with a rename, so a
    /// node never reads half a timestamp.
    pub fn set(&mut self, now: u64) -> io::Result<()> {
        if now != self.now {
            self.write(now)?;
        }
        Ok(())
    }

    fn write(&mut self, now: u64) -> io::Result<()> {
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, format!("{}\n", faketime_stamp(now)))?;
        fs::rename(&tmp, &self.path)?;
        self.now = now;
        Ok(())
    }
}

fn free_port() -> io::Result<SocketAddr> {
    TcpListener::bind("127.0.0.1:0")?.local_addr()
}

/// The config both nodes run with: only settings cachet 0.1 already had,
/// so the same file means the same thing to both binaries.
pub fn config(name: &str, listen: SocketAddr, admin: SocketAddr, origin: SocketAddr) -> String {
    format!(
        r#"node_id = "{name}"
listen = "{listen}"
admin_listen = "{admin}"

[[origins]]
name = "origin"
url = "http://{origin}"

[cache]
memory_bytes = {MEMORY_BYTES}
max_object_bytes = {MAX_OBJECT_BYTES}
shards = {SHARDS}
max_variants = 8
heuristic_max_secs = 86400

[health]
path = "/healthz"
interval_ms = 60000
timeout_ms = 5000
rise = 1
fall = 100

[timeouts]
connect_ms = 5000
response_ms = 30000
body_ms = 30000
client_header_ms = 60000
shutdown_ms = 2000
"#
    )
}

/// How many times [`Node::start`] tries fresh ports.
pub const START_ATTEMPTS: u32 = 5;

/// What both binaries log when a listener's port is taken
/// (`EADDRINUSE`'s message).
const ADDR_IN_USE: &str = "Address already in use";

/// How one start went.
enum Started {
    Up(Node),
    /// The node exited because a port was taken.
    PortTaken(Node),
}

/// Why a node did not come up.
enum Exited {
    PortTaken,
    Failed(io::Error),
}

/// A running node, killed when dropped.
#[derive(Debug)]
pub struct Node {
    pub name: String,
    pub proxy: SocketAddr,
    pub admin: SocketAddr,
    pub log: PathBuf,
    child: Child,
}

/// How to start a node.
#[derive(Debug, Clone)]
pub struct Launch<'a> {
    /// The binary: cachet 0.1 or the new one. Both take `--config FILE`.
    pub binary: &'a Path,
    /// The libfaketime shared object to preload.
    pub faketime: &'a Path,
    /// Where the config and the log go.
    pub dir: &'a Path,
    pub clock: &'a Clock,
    pub origin: SocketAddr,
}

impl Node {
    /// Starts `name` and waits until its admin listener answers. The
    /// node's name is the same for both binaries, since it appears in Via
    /// and Cache-Status.
    ///
    /// The ports are picked free and bound by the node a moment later, so
    /// another socket can take one in between. A node that exits because
    /// its port is in use is started again on fresh ports, up to
    /// [`START_ATTEMPTS`] times.
    pub fn start(name: &str, file: &str, launch: &Launch<'_>) -> io::Result<Node> {
        if !launch.faketime.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "libfaketime not found at {} (install libfaketime, or pass --faketime)",
                    launch.faketime.display()
                ),
            ));
        }
        let mut attempt = 1;
        loop {
            match Node::start_once(name, file, launch)? {
                Started::Up(node) => return Ok(node),
                Started::PortTaken(node) if attempt < START_ATTEMPTS => {
                    drop(node);
                    attempt += 1;
                }
                Started::PortTaken(node) => {
                    return Err(node.failed(&format!(
                        "found a port in use on each of {START_ATTEMPTS} starts"
                    )));
                }
            }
        }
    }

    fn start_once(name: &str, file: &str, launch: &Launch<'_>) -> io::Result<Started> {
        let proxy = free_port()?;
        let admin = free_port()?;
        let conf = launch.dir.join(format!("{file}.toml"));
        fs::write(&conf, config(name, proxy, admin, launch.origin))?;
        let log = launch.dir.join(format!("{file}.log"));
        let out = fs::File::create(&log)?;
        let child = Command::new(launch.binary)
            .arg("--config")
            .arg(&conf)
            .env("CACHET_ADMIN_TOKEN", super::log::ADMIN_TOKEN)
            .env_remove("CACHET_CLUSTER_SECRET")
            .env("CACHET_LOG", "warn")
            .env("TZ", "UTC")
            .env("LD_PRELOAD", launch.faketime)
            .env("FAKETIME_TIMESTAMP_FILE", launch.clock.path())
            .env("FAKETIME_NO_CACHE", "1")
            .env("DONT_FAKE_MONOTONIC", "1")
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out)
            .spawn()
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("cannot start {}: {e}", launch.binary.display()),
                )
            })?;
        let mut node = Node {
            name: file.to_string(),
            proxy,
            admin,
            log,
            child,
        };
        match node.wait_listening(Duration::from_secs(30)) {
            Ok(()) => Ok(Started::Up(node)),
            Err(Exited::PortTaken) => Ok(Started::PortTaken(node)),
            Err(Exited::Failed(e)) => Err(e),
        }
    }

    fn wait_listening(&mut self, limit: Duration) -> Result<(), Exited> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait().map_err(Exited::Failed)? {
                let log = fs::read_to_string(&self.log).unwrap_or_default();
                if log.contains(ADDR_IN_USE) {
                    return Err(Exited::PortTaken);
                }
                return Err(Exited::Failed(
                    self.failed(&format!("exited with {status}")),
                ));
            }
            let up = [self.proxy, self.admin].iter().all(|a| {
                std::net::TcpStream::connect_timeout(a, Duration::from_millis(200)).is_ok()
            });
            if up {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(Exited::Failed(self.failed("did not start listening")));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// An error naming the node with the end of its log.
    pub fn failed(&self, what: &str) -> io::Error {
        let log = fs::read_to_string(&self.log).unwrap_or_default();
        let tail: Vec<&str> = log.lines().rev().take(20).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        io::Error::other(format!(
            "{} {what}; its log ends:\n{}",
            self.name,
            tail.join("\n")
        ))
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_match_http_dates() {
        for secs in [
            0u64,
            86_399,
            951_782_400, // 2000-02-29
            1_767_225_600,
            1_794_182_400,
            4_102_444_799, // 2099-12-31 23:59:59
            1_234_567_890,
        ] {
            let stamp = faketime_stamp(secs);
            let http = httpdate::fmt_http_date(std::time::UNIX_EPOCH + Duration::from_secs(secs));
            let parsed = httpdate::parse_http_date(&http).unwrap();
            // Rebuild the stamp's fields from the HTTP date: "Sun, 06 Nov 1994 08:49:37 GMT".
            let f: Vec<&str> = http.split([' ', ':']).collect();
            let month = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ]
            .iter()
            .position(|m| *m == f[2])
            .unwrap()
                + 1;
            let want = format!("{}-{month:02}-{} {}:{}:{}", f[3], f[1], f[4], f[5], f[6]);
            assert_eq!(stamp, want, "{secs}");
            assert_eq!(
                parsed
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                secs
            );
        }
    }

    #[test]
    fn the_clock_file_moves() {
        let dir = std::env::temp_dir().join(format!("difftest-clock-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut clock = Clock::create(dir.join("now"), 0).unwrap();
        assert_eq!(
            fs::read_to_string(clock.path()).unwrap(),
            "1970-01-01 00:00:00\n"
        );
        clock.set(90_061).unwrap();
        assert_eq!(clock.now(), 90_061);
        assert_eq!(
            fs::read_to_string(clock.path()).unwrap(),
            "1970-01-02 01:01:01\n"
        );
        clock.set(90_061).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_config_parses_in_cachet() {
        let a: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let text = config("difftest", a, a, a);
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["cache"]["memory_bytes"].as_integer(), Some(4 << 30));
    }

    #[test]
    fn a_missing_faketime_or_binary_is_reported() {
        let dir = std::env::temp_dir().join(format!("difftest-node-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let clock = Clock::create(dir.join("now"), 0).unwrap();
        let origin: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let launch = Launch {
            binary: Path::new("/nonexistent/cachet"),
            faketime: Path::new("/nonexistent/libfaketime.so"),
            dir: &dir,
            clock: &clock,
            origin,
        };
        let err = Node::start("n", "n", &launch).unwrap_err();
        assert!(err.to_string().contains("libfaketime not found"), "{err}");
        // Any existing file passes the preload check; the binary is next.
        let here = std::env::current_exe().unwrap();
        let launch = Launch {
            faketime: &here,
            ..launch
        };
        let err = Node::start("n", "n", &launch).unwrap_err();
        assert!(
            err.to_string().contains("cannot start /nonexistent/cachet"),
            "{err}"
        );
        // A binary that exits at once is reported with its log.
        let launch = Launch {
            binary: Path::new("/bin/false"),
            faketime: Path::new("/dev/null"),
            ..launch
        };
        let err = Node::start("n", "n", &launch).unwrap_err();
        assert!(err.to_string().contains("n exited with"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_port_taken_on_every_start_is_reported_after_the_last_attempt() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("difftest-taken-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let clock = Clock::create(dir.join("now"), 0).unwrap();
        // Logs what a node logs when its listener's port is taken, counts
        // the start, and exits.
        let script = dir.join("taken");
        let starts = dir.join("starts");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho x >> {}\necho 'cannot listen on 127.0.0.1:1: Address already in use (os error 98)'\nexit 1\n",
                starts.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let launch = Launch {
            binary: &script,
            faketime: Path::new("/dev/null"),
            dir: &dir,
            clock: &clock,
            origin: "127.0.0.1:1".parse().unwrap(),
        };
        let err = Node::start("n", "n", &launch).unwrap_err();
        assert!(
            err.to_string()
                .contains("n found a port in use on each of 5 starts"),
            "{err}"
        );
        assert!(err.to_string().contains("Address already in use"), "{err}");
        let count = fs::read_to_string(&starts).unwrap().lines().count();
        assert_eq!(count as u32, START_ATTEMPTS);
        fs::remove_dir_all(&dir).unwrap();
    }
}
