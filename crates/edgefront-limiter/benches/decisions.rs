//! Single-thread decisions per second, with the workloads of libpacer's
//! bench/bench_limiter.c so the numbers compare directly: a rate of 10^7
//! per second with a burst of 1000, time advancing 50 ns per decision, and
//! a 200,000-entry table under the evict policy.
//!
//! Run with `cargo bench -p edgefront-limiter`; criterion reports time per
//! iteration and, through `Throughput::Elements`, decisions per second.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use edgefront_limiter::{FullPolicy, Gcra, KeyHash, Rate, Table, TableConfig};

const SEC: u64 = 1_000_000_000;
const RATE: Rate = Rate::new(10_000_000, SEC, 1000);
const BATCH: u64 = 10_000;

fn table(key_hash: KeyHash) -> Table {
    let mut cfg = TableConfig::new(200_000, RATE);
    cfg.full_policy = FullPolicy::Evict;
    cfg.key_hash = key_hash;
    Table::new(&cfg).unwrap()
}

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("decisions");
    group.throughput(Throughput::Elements(BATCH));

    let g = Gcra::new(&RATE).unwrap();
    let mut now = SEC;
    group.bench_function("gcra take", |b| {
        b.iter(|| {
            let mut allowed = 0u64;
            for _ in 0..BATCH {
                allowed += u64::from(g.take(black_box(now), 1).unwrap().allowed);
                now += 50;
            }
            allowed
        })
    });

    for (name, key_hash) in [("sip13", KeyHash::Sip13), ("sip24", KeyHash::Sip24)] {
        let t = table(key_hash);
        let mut now = SEC;
        let mut i = 0u32;
        group.bench_function(format!("table take, 10k keys, {name}"), |b| {
            b.iter(|| {
                let mut allowed = 0u64;
                for _ in 0..BATCH {
                    let k = i.wrapping_mul(2_654_435_761) % 10_000;
                    let d = t.take(black_box(&k.to_ne_bytes()), now, 1).unwrap();
                    allowed += u64::from(d.allowed);
                    i = i.wrapping_add(1);
                    now += 50;
                }
                allowed
            })
        });

        let t = table(key_hash);
        let mut now = SEC;
        group.bench_function(format!("table take, one hot key, {name}"), |b| {
            b.iter(|| {
                let mut allowed = 0u64;
                for _ in 0..BATCH {
                    allowed += u64::from(t.take(black_box(b"hot"), now, 1).unwrap().allowed);
                    now += 50;
                }
                allowed
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
