//! A boxed client's refusal: done-when item 1 of week 8 (under 200 ns,
//! without touching the limit table). 100,000 keys are boxed, then
//! 2,000,000 checks of random boxed keys are timed in batches of 32, and
//! the percentiles are of the per-check time of each batch.
//!
//! Run with `cargo bench -p edgefront-limiter --bench penalty`.

use std::hint::black_box;
use std::time::Instant;

use edgefront_limiter::penalty::{PenaltyBox, PenaltyConfig};

const KEYS: u64 = 100_000;
const CHECKS: usize = 2_000_000;
const BATCH: usize = 32;

fn main() {
    let sec = 1_000_000_000;
    let p = PenaltyBox::new(PenaltyConfig {
        windows: 1,
        window_ns: sec,
        timeout_ns: 3600 * sec,
        max_timeout_ns: 3600 * sec,
        forget_ns: 3600 * sec,
        capacity: 262_144,
    });
    let now = 10 * sec;
    for k in 0..KEYS {
        p.refused(&k.to_be_bytes(), now);
    }
    let boxed = p.boxed(now);
    let mut x = 88172645463325252u64;
    let mut samples = Vec::with_capacity(CHECKS / BATCH);
    let mut hits = 0u64;
    for _ in 0..CHECKS / BATCH {
        let keys: Vec<[u8; 8]> = (0..BATCH)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x % KEYS).to_be_bytes()
            })
            .collect();
        let t = Instant::now();
        for k in &keys {
            hits += u64::from(p.check(black_box(k), now).is_some());
        }
        samples.push(t.elapsed().as_nanos() as f64 / BATCH as f64);
    }
    samples.sort_by(f64::total_cmp);
    let pct = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    println!("{boxed} keys boxed; {hits} of {CHECKS} checks found their key boxed");
    println!(
        "boxed check, ns: p50 {:.1}, p99 {:.1}, p99.9 {:.1}",
        pct(0.5),
        pct(0.99),
        pct(0.999)
    );
}
