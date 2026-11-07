//! One pick of each balancing policy over 10 and 100 endpoints: a first
//! attempt (every endpoint a candidate, nothing tried) and a retry (one
//! endpoint already tried).

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use edgefront_core::clock::MonotonicClock;
use edgefront_proxy::balancer::Policy;
use edgefront_proxy::balancer::ewma::{Cost, LeastLoaded, PeakEwma, cost};
use edgefront_proxy::balancer::rng::SplitMix64;
use edgefront_proxy::balancer::rr::RoundRobin;

/// An endpoint as the least-loaded policy sees it.
struct Load {
    latency: PeakEwma,
    in_flight: AtomicI64,
}

impl Cost for Load {
    fn cost(&self, now: Instant) -> f64 {
        cost(
            self.latency.latency(now),
            self.in_flight.load(Ordering::Relaxed),
        )
    }
}

const SIZES: [usize; 2] = [10, 100];

fn round_robin(c: &mut Criterion) {
    let mut g = c.benchmark_group("round_robin");
    for n in SIZES {
        let weights: Vec<u32> = (0..n).map(|i| 1 + (i % 5) as u32).collect();
        let rr = RoundRobin::new(&weights);
        let cands: Vec<usize> = (0..n).collect();
        g.bench_with_input(BenchmarkId::new("first", n), &n, |b, _| {
            b.iter(|| rr.pick(black_box(&cands), black_box(&[]), None))
        });
        g.bench_with_input(BenchmarkId::new("retry", n), &n, |b, _| {
            b.iter(|| rr.pick(black_box(&cands), black_box(&[n / 2]), None))
        });
    }
    g.finish();
}

fn peak_ewma(c: &mut Criterion) {
    let mut g = c.benchmark_group("peak_ewma");
    for n in SIZES {
        let now = Instant::now();
        let eps: Arc<[Load]> = (0..n)
            .map(|i| {
                let latency = PeakEwma::new(now);
                latency.observe(Duration::from_micros(500 + 37 * i as u64), now);
                Load {
                    latency,
                    in_flight: AtomicI64::new((i % 7) as i64),
                }
            })
            .collect();
        let ll = LeastLoaded::new(
            eps,
            Arc::new(SplitMix64::new(n as u64)),
            MonotonicClock::shared(),
        );
        let cands: Vec<usize> = (0..n).collect();
        g.bench_with_input(BenchmarkId::new("first", n), &n, |b, _| {
            b.iter(|| ll.pick(black_box(&cands), black_box(&[]), None))
        });
        g.bench_with_input(BenchmarkId::new("retry", n), &n, |b, _| {
            b.iter(|| ll.pick(black_box(&cands), black_box(&[n / 2]), None))
        });
    }
    g.finish();
}

criterion_group!(benches, round_robin, peak_ewma);
criterion_main!(benches);
