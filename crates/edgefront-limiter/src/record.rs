//! A recorder of limit decisions, for replaying traffic against a
//! candidate policy ([`crate::replay`]).
//!
//! Each decision is one fixed 32-byte [`Record`]: the time (ns on the
//! limiter's clock), the rule's index, the cost, the outcome, and a
//! 128-bit hash of the key, keyed by two random SipHash keys the recorder
//! picks and never writes down. A recording therefore holds no key, API
//! key or cookie, not even a short one, and a replay still tells keys
//! apart, since equal keys hash equal within one recorder. Records
//! collect in a bounded in-memory ring; [`Recorder::flush`] writes what is
//! there to the current file and empties it, and a file that would pass
//! `file_bytes` is closed and the next one opened (`decisions-000001.rec`
//! and on), keeping at most `files` files: rotation drops the oldest. A
//! ring that fills before a flush drops the oldest records and counts
//! them in [`Recorder::dropped`], so recording never blocks or grows.
//!
//! The file format is a 16-byte header, the magic `EFREC001` and the
//! rule names' count, then each rule name as a length byte and its bytes,
//! then records back to back, little-endian:
//!
//! | bytes | field |
//! |---|---|
//! | 0..8 | time in ns |
//! | 8..10 | rule index |
//! | 10 | outcome: 0 allowed, 1 refused, 2 would refuse (shadow) |
//! | 11 | zero |
//! | 12..16 | cost (saturating at 2^32 - 1) |
//! | 16..32 | the key's hash |

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::siphash::{KEY_LEN, random_key, siphash13};

pub const MAGIC: &[u8; 8] = b"EFREC001";
pub const RECORD_BYTES: usize = 32;

/// What the recorded rule did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Allowed,
    Refused,
    /// A shadow rule would have refused it.
    WouldRefuse,
}

impl Outcome {
    fn code(self) -> u8 {
        match self {
            Outcome::Allowed => 0,
            Outcome::Refused => 1,
            Outcome::WouldRefuse => 2,
        }
    }

    fn from_code(c: u8) -> Option<Outcome> {
        Some(match c {
            0 => Outcome::Allowed,
            1 => Outcome::Refused,
            2 => Outcome::WouldRefuse,
            _ => return None,
        })
    }

    /// Whether the limit, enforcing, refused it.
    pub fn refused(self) -> bool {
        self != Outcome::Allowed
    }
}

/// One decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub at_ns: u64,
    pub rule: u16,
    pub outcome: Outcome,
    pub cost: u32,
    /// The key's 128-bit keyed hash.
    pub key: [u8; 16],
}

impl Record {
    /// A record whose key is already hashed.
    pub fn new(at_ns: u64, rule: u16, key: [u8; 16], cost: u64, outcome: Outcome) -> Record {
        Record {
            at_ns,
            rule,
            outcome,
            cost: u32::try_from(cost).unwrap_or(u32::MAX),
            key,
        }
    }

    pub fn encode(&self, out: &mut [u8; RECORD_BYTES]) {
        out[0..8].copy_from_slice(&self.at_ns.to_le_bytes());
        out[8..10].copy_from_slice(&self.rule.to_le_bytes());
        out[10] = self.outcome.code();
        out[11] = 0;
        out[12..16].copy_from_slice(&self.cost.to_le_bytes());
        out[16..32].copy_from_slice(&self.key);
    }

    pub fn decode(b: &[u8; RECORD_BYTES]) -> Option<Record> {
        if b[11] != 0 {
            return None;
        }
        Some(Record {
            at_ns: u64::from_le_bytes(b[0..8].try_into().ok()?),
            rule: u16::from_le_bytes(b[8..10].try_into().ok()?),
            outcome: Outcome::from_code(b[10])?,
            cost: u32::from_le_bytes(b[12..16].try_into().ok()?),
            key: b[16..32].try_into().ok()?,
        })
    }
}

/// Where and how much to record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordConfig {
    pub dir: PathBuf,
    /// Records held in memory between flushes.
    pub ring: usize,
    /// A file is closed once it holds this many bytes.
    pub file_bytes: u64,
    /// At most this many files are kept.
    pub files: usize,
}

/// The `[recording]` section of a limiter config.
///
/// ```toml
/// [recording]
/// dir = "recordings"        # beside the config file when relative
/// ring = 65536              # records held between flushes
/// file_bytes = 67108864     # a file is closed past this size
/// files = 16                # files kept
/// ```
#[derive(Clone, Debug, Default, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordToml {
    pub dir: PathBuf,
    #[serde(default)]
    pub ring: Option<usize>,
    #[serde(default)]
    pub file_bytes: Option<u64>,
    #[serde(default)]
    pub files: Option<usize>,
}

/// A document's `recording` table, ignoring everything else.
#[derive(Clone, Debug, Default, serde::Deserialize, PartialEq, Eq)]
pub struct RecordSection {
    #[serde(default)]
    pub recording: Option<RecordToml>,
}

impl RecordToml {
    /// The config, with a relative `dir` resolved against `base`.
    pub fn build(&self, base: &Path) -> Result<RecordConfig, String> {
        let ring = self.ring.unwrap_or(65536);
        let file_bytes = self.file_bytes.unwrap_or(64 << 20);
        let files = self.files.unwrap_or(16);
        if ring == 0 || files == 0 || file_bytes < 4096 {
            return Err(
                "recording: ring and files must be at least 1 and file_bytes at least 4096".into(),
            );
        }
        Ok(RecordConfig {
            dir: if self.dir.is_absolute() {
                self.dir.clone()
            } else {
                base.join(&self.dir)
            },
            ring,
            file_bytes,
            files,
        })
    }
}

struct Writer {
    file: Option<std::fs::File>,
    written: u64,
    seq: u64,
}

/// The recorder.
pub struct Recorder {
    cfg: RecordConfig,
    hash_keys: ([u8; KEY_LEN], [u8; KEY_LEN]),
    rules: Vec<String>,
    ring: Mutex<VecDeque<Record>>,
    writer: Mutex<Writer>,
    dropped: AtomicU64,
    recorded: AtomicU64,
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder")
            .field("dir", &self.cfg.dir)
            .finish()
    }
}

fn header(rules: &[String]) -> Vec<u8> {
    let mut h = Vec::with_capacity(16 + rules.len() * 16);
    h.extend_from_slice(MAGIC);
    h.extend_from_slice(&(rules.len() as u64).to_le_bytes());
    for r in rules {
        let b = &r.as_bytes()[..r.len().min(255)];
        h.push(b.len() as u8);
        h.extend_from_slice(b);
    }
    h
}

impl Recorder {
    /// A recorder for the rules named `rules` (by index), creating `dir`.
    pub fn new(cfg: RecordConfig, rules: Vec<String>) -> std::io::Result<Self> {
        std::fs::create_dir_all(&cfg.dir)?;
        let seq = files_in(&cfg.dir)?.last().map_or(0, |(n, _)| *n);
        Ok(Recorder {
            hash_keys: (random_key(), random_key()),
            ring: Mutex::new(VecDeque::with_capacity(cfg.ring.max(1))),
            cfg,
            rules,
            writer: Mutex::new(Writer {
                file: None,
                written: 0,
                seq,
            }),
            dropped: AtomicU64::new(0),
            recorded: AtomicU64::new(0),
        })
    }

    /// The 128-bit hash this recorder writes for `key`.
    #[inline]
    pub fn hash(&self, key: &[u8]) -> [u8; 16] {
        let mut h = [0u8; 16];
        h[..8].copy_from_slice(&siphash13(&self.hash_keys.0, key).to_le_bytes());
        h[8..].copy_from_slice(&siphash13(&self.hash_keys.1, key).to_le_bytes());
        h
    }

    /// Adds one decision to the ring.
    #[inline]
    pub fn record(&self, r: Record) {
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        if ring.len() >= self.cfg.ring.max(1) {
            ring.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        ring.push_back(r);
        self.recorded.fetch_add(1, Ordering::Relaxed);
    }

    /// Records dropped because the ring was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn recorded(&self) -> u64 {
        self.recorded.load(Ordering::Relaxed)
    }

    /// Writes the ring out and empties it; how many records.
    pub fn flush(&self) -> std::io::Result<usize> {
        let records: Vec<Record> = {
            let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
            ring.drain(..).collect()
        };
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let mut buf = Vec::with_capacity(records.len() * RECORD_BYTES);
        let mut rec = [0u8; RECORD_BYTES];
        for r in &records {
            r.encode(&mut rec);
            buf.extend_from_slice(&rec);
        }
        let mut at = 0;
        while at < buf.len() {
            if w.file.is_none() || w.written >= self.cfg.file_bytes {
                w.seq += 1;
                let path = self.cfg.dir.join(format!("decisions-{:06}.rec", w.seq));
                let mut f = std::fs::File::create(&path)?;
                let h = header(&self.rules);
                f.write_all(&h)?;
                w.written = h.len() as u64;
                w.file = Some(f);
                let files = files_in(&self.cfg.dir)?;
                if files.len() > self.cfg.files.max(1) {
                    for (_, p) in &files[..files.len() - self.cfg.files.max(1)] {
                        let _ = std::fs::remove_file(p);
                    }
                }
            }
            let room = self
                .cfg
                .file_bytes
                .saturating_sub(w.written)
                .max(RECORD_BYTES as u64) as usize;
            let room = room - room % RECORD_BYTES;
            let end = (at + room).min(buf.len());
            let f = w.file.as_mut().expect("opened above");
            f.write_all(&buf[at..end])?;
            w.written += (end - at) as u64;
            at = end;
        }
        if let Some(f) = w.file.as_mut() {
            f.flush()?;
        }
        Ok(records.len())
    }
}

/// The recording files in `dir`, by sequence number.
pub fn files_in(dir: &Path) -> std::io::Result<Vec<(u64, PathBuf)>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        let n = p.file_name().and_then(|n| n.to_str()).and_then(|n| {
            n.strip_prefix("decisions-")?
                .strip_suffix(".rec")?
                .parse::<u64>()
                .ok()
        });
        if let Some(n) = n {
            out.push((n, p));
        }
    }
    out.sort();
    Ok(out)
}

/// One recording file read back: its rule names and records.
#[derive(Debug, Default)]
pub struct Recording {
    pub rules: Vec<String>,
    pub records: Vec<Record>,
}

/// Reads a recording file, or every file of a directory in order (their
/// rule names must agree).
pub fn read(path: &Path) -> Result<Recording, String> {
    let files = if path.is_dir() {
        files_in(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .into_iter()
            .map(|(_, p)| p)
            .collect()
    } else {
        vec![path.to_path_buf()]
    };
    if files.is_empty() {
        return Err(format!("{}: no recording files", path.display()));
    }
    let mut out = Recording::default();
    for f in files {
        let mut bytes = Vec::new();
        std::fs::File::open(&f)
            .and_then(|mut h| h.read_to_end(&mut bytes))
            .map_err(|e| format!("{}: {e}", f.display()))?;
        let bad = |why: &str| format!("{}: {why}", f.display());
        if bytes.len() < 16 || &bytes[..8] != MAGIC {
            return Err(bad("not a recording (bad magic)"));
        }
        let n = u64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes")) as usize;
        let mut at = 16;
        let mut rules = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            let len = *bytes.get(at).ok_or_else(|| bad("truncated header"))? as usize;
            let name = bytes
                .get(at + 1..at + 1 + len)
                .ok_or_else(|| bad("truncated header"))?;
            rules.push(String::from_utf8_lossy(name).into_owned());
            at += 1 + len;
        }
        if out.rules.is_empty() {
            out.rules = rules;
        } else if out.rules != rules {
            return Err(bad("its rules differ from the files before it"));
        }
        let body = &bytes[at..];
        if body.len() % RECORD_BYTES != 0 {
            return Err(bad("truncated record"));
        }
        for chunk in body.as_chunks::<RECORD_BYTES>().0 {
            let r = Record::decode(chunk).ok_or_else(|| bad("bad record"))?;
            out.records.push(r);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rec-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_section_builds() {
        let s: RecordSection = toml::from_str("[recording]\ndir = \"rec\"\nfiles = 2\n").unwrap();
        let c = s.recording.unwrap().build(Path::new("/etc/edge")).unwrap();
        assert_eq!(
            c,
            RecordConfig {
                dir: "/etc/edge/rec".into(),
                ring: 65536,
                file_bytes: 64 << 20,
                files: 2
            }
        );
        let s: RecordSection = toml::from_str("[recording]\ndir = \"/r\"\nring = 0\n").unwrap();
        assert!(s.recording.unwrap().build(Path::new(".")).is_err());
        let s: RecordSection = toml::from_str("[recording]\ndir = \"/r\"\n").unwrap();
        assert_eq!(
            s.recording.unwrap().build(Path::new(".")).unwrap().dir,
            PathBuf::from("/r")
        );
    }

    #[test]
    fn records_round_trip() {
        let r = Record::new(123, 7, [5; 16], 9, Outcome::WouldRefuse);
        let mut b = [0u8; RECORD_BYTES];
        r.encode(&mut b);
        assert_eq!(Record::decode(&b), Some(r));
        b[10] = 9;
        assert_eq!(Record::decode(&b), None);
        b[10] = 0;
        b[11] = 99;
        assert_eq!(Record::decode(&b), None);
        let big = Record::new(0, 0, [0; 16], u64::MAX, Outcome::Allowed);
        assert_eq!(big.cost, u32::MAX);
        assert!(Outcome::Refused.refused() && !Outcome::Allowed.refused());
    }

    #[test]
    fn the_ring_is_bounded_and_files_rotate() {
        let d = dir("rotate");
        let cfg = RecordConfig {
            dir: d.clone(),
            ring: 1000,
            file_bytes: 16 + 4 + 100 * RECORD_BYTES as u64,
            files: 3,
        };
        let rec = Recorder::new(cfg, vec!["api".into()]).unwrap();
        let (a, b) = (rec.hash(b"secret-api-key"), rec.hash(b"secret-api-key"));
        assert_eq!(a, b);
        assert_ne!(a, rec.hash(b"other"));
        for i in 0..1500u64 {
            rec.record(Record::new(
                i,
                0,
                rec.hash(&i.to_be_bytes()),
                1,
                Outcome::Allowed,
            ));
        }
        assert_eq!(rec.dropped(), 500);
        assert_eq!(rec.recorded(), 1500);
        assert_eq!(rec.flush().unwrap(), 1000);
        let files = files_in(&d).unwrap();
        assert_eq!(files.len(), 3, "older files dropped");
        let back = read(&d).unwrap();
        assert_eq!(back.rules, ["api"]);
        assert_eq!(back.records.len(), 300);
        assert_eq!(back.records.last().unwrap().at_ns, 1499);
        // A fresh recorder in the same directory continues the numbering.
        let rec2 = Recorder::new(
            RecordConfig {
                dir: d.clone(),
                ring: 10,
                file_bytes: 1 << 20,
                files: 10,
            },
            vec!["api".into()],
        )
        .unwrap();
        rec2.record(Record::new(2000, 0, rec2.hash(b"x"), 1, Outcome::Refused));
        rec2.flush().unwrap();
        assert_eq!(files_in(&d).unwrap().last().unwrap().0, 11);
        // Bad files are refused with the reason.
        let bad = d.join("decisions-999999.rec");
        std::fs::write(&bad, b"nope").unwrap();
        assert!(read(&bad).unwrap_err().contains("bad magic"));
        let mut h = header(&["api".into()]);
        h.extend_from_slice(&[0; 10]);
        std::fs::write(&bad, &h).unwrap();
        assert!(read(&bad).unwrap_err().contains("truncated record"));
        assert!(read(&dir("none")).is_err());
        assert!(format!("{rec:?}").contains("Recorder"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
