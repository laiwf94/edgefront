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
    endpoints: Vec<FileEndpoint>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileEndpoint {
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

/// Parses an endpoint file's contents.
pub fn parse(data: &[u8]) -> Result<Vec<Endpoint>, String> {
    let set: FileSet = serde_json::from_slice(data).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(set.endpoints.len());
    for (i, e) in set.endpoints.into_iter().enumerate() {
        let port_ok = e.address.rsplit_once(':').is_some_and(|(host, port)| {
            !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p > 0)
        });
        if !port_ok {
            return Err(format!(
                "endpoints[{i}].address {:?} is not host:port",
                e.address
            ));
        }
        if !(1..=1000).contains(&e.weight) {
            return Err(format!("endpoints[{i}].weight must be between 1 and 1000"));
        }
        if e.priority > 7 {
            return Err(format!("endpoints[{i}].priority must be at most 7"));
        }
        out.push(Endpoint {
            address: e.address,
            weight: e.weight,
            priority: e.priority,
            zone: e.zone,
            ready: e.ready,
        });
    }
    Ok(out)
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
