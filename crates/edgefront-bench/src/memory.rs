//! Memory per tracked client, for libpacer's keyed table and for
//! edgefront-limiter's: each one is driven through the same operation
//! protocol (`legacy/pacer/tests/diffdrive.c` for the C, and
//! `edgefront-difftest pacer-rust-driver` for the Rust), in a process of
//! its own. The process's resident memory is read before the table is
//! made, after it is made, and after `keys` clients have each taken a
//! token, and the difference is divided by the clients.
//!
//! Both tables allocate their slots when they are made, so most of the
//! memory is in the "made" step, and the operating system only backs the
//! pages once they are written; the "filled" figure is what a table full
//! of clients really costs. Every client takes one token of a rate of one
//! per second, so none is idle. The table is made for `keys` clients
//! under the evict policy and offered all of them; a shard that fills
//! before the others evicts, so the clients it ends up holding (its
//! `entries`, read back from the driver) are what the memory is divided
//! by.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde::Serialize;

/// One implementation's figures.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Measure {
    pub implementation: String,
    /// Clients the table held once filled.
    pub keys: u64,
    /// Resident memory, KiB, before the table was made.
    pub rss_start_kib: u64,
    /// After it was made.
    pub rss_made_kib: u64,
    /// After `keys` clients were added.
    pub rss_filled_kib: u64,
}

impl Measure {
    /// Bytes per client of a filled table: (filled - start) / keys.
    pub fn bytes_per_client(&self) -> f64 {
        (self.rss_filled_kib.saturating_sub(self.rss_start_kib) * 1024) as f64 / self.keys as f64
    }
}

/// A process's resident memory in KiB: /proc on Linux, `ps` elsewhere.
pub fn rss_kib(pid: u32) -> Result<u64, String> {
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        return status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| "no VmRSS line".to_string());
    }
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .map_err(|e| format!("ps: {e}"))?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|_| format!("no rss for {pid}"))
}

/// The operations that fill a table of `keys` clients: one take each,
/// with 8-byte keys, after the table is made with that many entries.
fn script(keys: u64) -> (String, String) {
    let make = format!(
        "table 0 create {keys} 0 1 1000000000 1 evict 000102030405060708090a0b0c0d0e0f\nsync\n"
    );
    let mut fill = String::with_capacity(keys as usize * 32);
    for k in 0..keys {
        fill.push_str(&format!("table 0 take {k:016x} 1\n"));
    }
    fill.push_str("table 0 stats\nsync\n");
    (make, fill)
}

/// Drives `cmd` (a diffdrive-protocol driver) through making and filling a
/// table of `keys` clients, reading its memory at each step.
pub fn measure(name: &str, mut cmd: Command, keys: u64) -> Result<Measure, String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("{name}: {e}"))?;
    let pid = child.id();
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let mut out = BufReader::new(child.stdout.take().ok_or("no stdout")?);
    let mut line = String::new();
    // Wait for a `sync` line, skipping the answers before it, and keep
    // the `entries=` of a stats line on the way.
    let mut entries = None;
    let mut wait_sync = |out: &mut BufReader<_>, entries: &mut Option<u64>| -> Result<(), String> {
        loop {
            line.clear();
            if out.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                return Err(format!("{name} ended early"));
            }
            let l = line.trim_end();
            if l == "sync" {
                return Ok(());
            }
            if let Some(rest) = l.strip_prefix("entries=") {
                *entries = rest.split(' ').next().and_then(|v| v.parse().ok());
            }
            if l.contains("EINVAL") || l.contains("EFULL") {
                return Err(format!("{name}: {l}"));
            }
        }
    };
    stdin.write_all(b"sync\n").map_err(|e| e.to_string())?;
    stdin.flush().map_err(|e| e.to_string())?;
    wait_sync(&mut out, &mut entries)?;
    let rss_start_kib = rss_kib(pid)?;
    let (make, fill) = script(keys);
    stdin
        .write_all(make.as_bytes())
        .map_err(|e| e.to_string())?;
    stdin.flush().map_err(|e| e.to_string())?;
    wait_sync(&mut out, &mut entries)?;
    let rss_made_kib = rss_kib(pid)?;
    // A writer thread, so a long script never blocks on a full pipe.
    let writer = std::thread::spawn(move || {
        let r = stdin
            .write_all(fill.as_bytes())
            .and_then(|()| stdin.flush());
        (r, stdin)
    });
    wait_sync(&mut out, &mut entries)?;
    let rss_filled_kib = rss_kib(pid)?;
    let (r, stdin) = writer.join().map_err(|_| "writer panicked")?;
    r.map_err(|e| e.to_string())?;
    drop(stdin);
    let _ = child.wait();
    Ok(Measure {
        implementation: name.to_string(),
        keys: entries.unwrap_or(keys),
        rss_start_kib,
        rss_made_kib,
        rss_filled_kib,
    })
}

/// The Markdown table for any number of measures.
pub fn markdown(ms: &[Measure]) -> String {
    let mut out = String::from(
        "| implementation | clients | RSS before (KiB) | table made (KiB) | filled (KiB) | bytes per client |\n|---|---:|---:|---:|---:|---:|\n",
    );
    for m in ms {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {:.1} |\n",
            m.implementation,
            m.keys,
            m.rss_start_kib,
            m.rss_made_kib,
            m.rss_filled_kib,
            m.bytes_per_client()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_has_a_resident_size() {
        assert!(rss_kib(std::process::id()).unwrap() > 0);
        assert!(rss_kib(u32::MAX).is_err());
    }

    #[test]
    fn the_script_makes_then_fills() {
        let (make, fill) = script(3);
        assert!(make.starts_with("table 0 create 3 0 1 1000000000 1 evict "));
        assert!(make.ends_with("\nsync\n"));
        assert_eq!(
            fill,
            "table 0 take 0000000000000000 1\ntable 0 take 0000000000000001 1\ntable 0 take 0000000000000002 1\ntable 0 stats\nsync\n"
        );
    }

    #[test]
    fn a_driver_that_echoes_sync_is_measured() {
        // A shell loop that answers the sync lines and a stats line: enough
        // to walk the steps.
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "while read l; do case \"$l\" in sync) echo sync;; *stats) echo entries=990 capacity=1000;; esac; done",
        ]);
        let m = measure("sh", cmd, 1000).unwrap();
        assert_eq!(m.keys, 990);
        assert!(m.rss_start_kib > 0);
        let md = markdown(&[m]);
        assert!(md.contains("| sh | 990 |"), "{md}");
        // A driver that refuses the table is an error, as is one that ends.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "read l; echo sync; read l; echo rc=EINVAL"]);
        assert!(measure("refuses", cmd, 10).unwrap_err().contains("EINVAL"));
        let mut cmd = Command::new("true");
        cmd.arg("x");
        assert!(measure("ends", cmd, 10).is_err());
        assert!(measure("missing", Command::new("/nonexistent/driver"), 1).is_err());
    }

    #[test]
    fn bytes_per_client_is_the_growth_over_the_clients() {
        let m = Measure {
            implementation: "x".into(),
            keys: 1024,
            rss_start_kib: 100,
            rss_made_kib: 150,
            rss_filled_kib: 164,
        };
        assert_eq!(m.bytes_per_client(), 64.0);
    }
}
