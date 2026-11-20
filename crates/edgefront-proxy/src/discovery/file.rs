//! Endpoints from a watched file.
//!
//! The file is JSON: `{"endpoints": [{"address": "10.0.0.1:8080",
//! "weight": 2, "priority": 0, "zone": "a", "ready": true}]}`, where only
//! `address` is required. It is checked every `interval` by its identity
//! (device, inode, size and modification time), so both ways of replacing
//! it are seen: an edit in place, and the atomic write-then-rename that
//! config tools use, which swaps the inode under the same name.
//!
//! A change is read only once the file has stayed the same for
//! `debounce`, so a burst of edits yields one read of the final content,
//! not one per edit. A file that cannot be read or parsed (missing, half
//! written, not JSON, an address without a port) is reported as a
//! failure, so the cluster keeps the last set it read ([`super::Merger`]);
//! an identical re-read sends nothing.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use tokio::sync::mpsc;

use super::{Endpoint, Source, UpdateKind};

/// The largest endpoint file read, 16 MiB.
pub const MAX_FILE_BYTES: u64 = 16 << 20;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSet {
    endpoints: Vec<WireEndpoint>,
}

/// One endpoint as files and the delta feed write it: only `address` is
/// required.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WireEndpoint {
    address: String,
    #[serde(default = "one")]
    weight: u32,
    #[serde(default)]
    priority: u8,
    #[serde(default)]
    zone: Option<String>,
    #[serde(default = "yes")]
    ready: bool,
}

fn one() -> u32 {
    1
}

fn yes() -> bool {
    true
}

impl WireEndpoint {
    /// The endpoint, checked as the configuration's are: `host:port`
    /// with a nonzero port, weight 1 to 1,000 and priority up to 7. `at`
    /// names it in errors.
    pub(super) fn check(self, at: &str) -> Result<Endpoint, String> {
        let port_ok = self.address.rsplit_once(':').is_some_and(|(host, port)| {
            !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p > 0)
        });
        if !port_ok {
            return Err(format!("{at}.address {:?} is not host:port", self.address));
        }
        if !(1..=1000).contains(&self.weight) {
            return Err(format!("{at}.weight must be between 1 and 1000"));
        }
        if self.priority > 7 {
            return Err(format!("{at}.priority must be at most 7"));
        }
        Ok(Endpoint {
            address: self.address,
            weight: self.weight,
            priority: self.priority,
            zone: self.zone,
            ready: self.ready,
        })
    }
}

/// Parses an endpoint file's contents.
pub fn parse(data: &[u8]) -> Result<Vec<Endpoint>, String> {
    let set: FileSet = serde_json::from_slice(data).map_err(|e| e.to_string())?;
    set.endpoints
        .into_iter()
        .enumerate()
        .map(|(i, e)| e.check(&format!("endpoints[{i}]")))
        .collect()
}

/// What identifies one version of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    len: u64,
    modified: Option<SystemTime>,
}

fn stamp(path: &PathBuf) -> std::io::Result<Stamp> {
    let m = std::fs::metadata(path)?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(Stamp {
        #[cfg(unix)]
        dev: m.dev(),
        #[cfg(unix)]
        ino: m.ino(),
        len: m.len(),
        modified: m.modified().ok(),
    })
}

fn read(path: &PathBuf) -> Result<Vec<Endpoint>, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut data = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if data.len() as u64 > MAX_FILE_BYTES {
        return Err(format!(
            "{}: larger than {MAX_FILE_BYTES} bytes",
            path.display()
        ));
    }
    parse(&data).map_err(|e| format!("{}: {e}", path.display()))
}

/// A watched endpoint file.
#[derive(Debug, Clone)]
pub struct FileSource {
    pub path: PathBuf,
    /// How often the file's identity is checked.
    pub interval: Duration,
    /// How long it must stay the same before it is read.
    pub debounce: Duration,
}

impl FileSource {
    /// Checks every 100 ms and reads after 50 ms of quiet.
    pub fn new(path: impl Into<PathBuf>) -> FileSource {
        FileSource {
            path: path.into(),
            interval: Duration::from_millis(100),
            debounce: Duration::from_millis(50),
        }
    }

    async fn watch(self, tx: mpsc::Sender<UpdateKind>) {
        let mut seen: Option<Stamp> = None;
        let mut sent: Option<Result<Vec<Endpoint>, String>> = None;
        loop {
            let now = stamp(&self.path);
            let changed = match (&now, &seen) {
                (Ok(a), Some(b)) => a != b,
                _ => true,
            };
            if changed {
                // Debounce: wait until the file holds still.
                let mut last = now;
                loop {
                    tokio::time::sleep(self.debounce).await;
                    let again = stamp(&self.path);
                    let still = match (&again, &last) {
                        (Ok(a), Ok(b)) => a == b,
                        (Err(_), Err(_)) => true,
                        _ => false,
                    };
                    last = again;
                    if still {
                        break;
                    }
                }
                let result = match &last {
                    Ok(_) => read(&self.path),
                    Err(e) => Err(format!("{}: {e}", self.path.display())),
                };
                seen = last.ok();
                if sent.as_ref() != Some(&result) {
                    let kind = match &result {
                        Ok(list) => UpdateKind::Set(list.clone()),
                        Err(e) => UpdateKind::Failed(e.clone()),
                    };
                    if tx.send(kind).await.is_err() {
                        return;
                    }
                    sent = Some(result);
                }
            }
            tokio::select! {
                () = tokio::time::sleep(self.interval) => {}
                () = tx.closed() => return,
            }
        }
    }
}

impl Source for FileSource {
    fn name(&self) -> String {
        format!("file:{}", self.path.display())
    }

    fn run(
        self: Box<Self>,
        tx: mpsc::Sender<UpdateKind>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin((*self).watch(tx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_parse_with_defaults_and_checks() {
        let list = parse(
            br#"{"endpoints":[{"address":"10.0.0.1:80"},
                {"address":"[::1]:8080","weight":3,"priority":1,"zone":"b","ready":false}]}"#,
        )
        .unwrap();
        assert_eq!(list[0], Endpoint::new("10.0.0.1:80"));
        assert_eq!(list[1].weight, 3);
        assert_eq!(list[1].zone.as_deref(), Some("b"));
        assert!(!list[1].ready);
        for (bad, want) in [
            (&br#"{"endpoints":[{"address":"h"}]}"#[..], "not host:port"),
            (br#"{"endpoints":[{"address":"h:0"}]}"#, "not host:port"),
            (br#"{"endpoints":[{"address":":80"}]}"#, "not host:port"),
            (br#"{"endpoints":[{"address":"h:1","weight":0}]}"#, "weight"),
            (
                br#"{"endpoints":[{"address":"h:1","priority":8}]}"#,
                "priority",
            ),
            (
                br#"{"endpoints":[{"address":"h:1","x":1}]}"#,
                "unknown field",
            ),
            (br#"{"endpoints":[{"addr"#, "EOF"),
        ] {
            let err = parse(bad).unwrap_err();
            assert!(err.contains(want), "{err}");
        }
        assert_eq!(parse(br#"{"endpoints":[]}"#).unwrap(), Vec::new());
    }
}
