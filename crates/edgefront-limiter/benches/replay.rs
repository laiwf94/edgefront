//! Replaying an hour of recorded traffic, 10,000,000 requests, against a
//! candidate policy: done-when item 2 of week 9 (under 60 s).
//!
//! The recording is written by a [`Recorder`] as the proxy would write
//! it: 10,000,000 decisions spread evenly over 3,600 s, from 100,000 keys
//! chosen with a skew (low keys far more often), under a limit of 5 a
//! second with a burst of 10. The timed part reads the files back and
//! replays them against a limit of 2 a second with a burst of 5, the way
//! `edgefront ctl limits replay` does.
//!
//! Run with `cargo bench -p edgefront-limiter --bench replay`.

use std::time::Instant;

use edgefront_limiter::config::http_limits::{HttpLimitsSection, build};
use edgefront_limiter::record::{Outcome, Record, RecordConfig, Recorder, read};
use edgefront_limiter::replay::replay;
use edgefront_limiter::{Rate, Table, TableConfig};

const RECORDS: u64 = 10_000_000;
const KEYS: u64 = 100_000;
const HOUR_NS: u64 = 3600 * 1_000_000_000;

fn main() {
    let dir = std::env::temp_dir().join(format!("replay-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let rec = Recorder::new(
        RecordConfig {
            dir: dir.clone(),
            ring: 1 << 20,
            file_bytes: 256 << 20,
            files: 64,
        },
        vec!["api".into()],
    )
    .unwrap();
    // The recorded limit decides as the proxy did.
    let live = Table::new(&TableConfig::new(1 << 18, Rate::new(5, 1_000_000_000, 10))).unwrap();
    let mut x = 88172645463325252u64;
    let t0 = Instant::now();
    for i in 0..RECORDS {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let r = x % 1_000_000;
        let k = (r * r / 10_000_000) % KEYS;
        let at = 1_000_000_000 + i * (HOUR_NS / RECORDS);
        let kb = k.to_be_bytes();
        let allowed = live.take(&kb, at, 1).map(|d| d.allowed).unwrap_or(false);
        let outcome = if allowed {
            Outcome::Allowed
        } else {
            Outcome::Refused
        };
        rec.record(Record::new(at, 0, rec.hash(&kb), 1, outcome));
        if i % (1 << 19) == 0 {
            rec.flush().unwrap();
        }
    }
    rec.flush().unwrap();
    let written = t0.elapsed().as_secs_f64();
    let bytes: u64 = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    let candidate: HttpLimitsSection = toml::from_str(
        "[[http_limits]]\nname = \"api\"\nrate = \"2/s burst 5\"\ntable_size = 262144\n",
    )
    .unwrap();
    let candidate = build(&candidate.http_limits).unwrap();
    let t1 = Instant::now();
    let recording = read(&dir).unwrap();
    let read_s = t1.elapsed().as_secs_f64();
    let report = replay(&recording, &candidate).unwrap();
    let total = t1.elapsed().as_secs_f64();
    let d = &report.rules[0];
    println!(
        "{} records, {:.0} MiB in {} files, written in {written:.1} s",
        recording.records.len(),
        bytes as f64 / (1 << 20) as f64,
        std::fs::read_dir(&dir).unwrap().count()
    );
    println!(
        "read {read_s:.2} s, read and replayed {total:.2} s; refused before {}, after {}; {} of {} keys changed, all listed: {}",
        d.refused_before,
        d.refused_after,
        d.keys_changed,
        d.keys,
        report.keys.len() as u64 == d.keys_changed
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
