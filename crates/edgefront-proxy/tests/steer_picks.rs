//! Round robin, power of two choices and their random source against steer.
//!
//! `fixtures/steer_picks.txt` was written by `TestPickVectors` in
//! `legacy/steer/cmd/diffdrive`, which drives steer's own
//! `balancer.RoundRobin` and `balancer.LeastLoaded` with a SplitMix64 per
//! case and records every pick; that test also fails if steer would now
//! write anything else. Here each case is replayed against the Rust
//! policies with the same seed, and every pick must match.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use edgefront_core::clock::{Clock, ManualClock};
use edgefront_proxy::balancer::Policy;
use edgefront_proxy::balancer::ewma::{Cost, LeastLoaded, PeakEwma, cost};
use edgefront_proxy::balancer::rng::{Rng, SplitMix64};
use edgefront_proxy::balancer::rr::RoundRobin;

const FIXTURE: &str = include_str!("fixtures/steer_picks.txt");

#[derive(Debug)]
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

fn list(s: &str) -> Vec<usize> {
    if s == "-" {
        return Vec::new();
    }
    s.split(',').map(|x| x.parse().unwrap()).collect()
}

struct Pick {
    cands: Vec<usize>,
    tried: Vec<usize>,
    want: Option<usize>,
}

fn parse_pick(f: &[&str], n: usize) -> Pick {
    assert_eq!((f.len(), f[3]), (5, "->"), "pick line {f:?}");
    let cands = if f[1] == "*" {
        (0..n).collect()
    } else {
        list(f[1])
    };
    let want = match f[4] {
        "-1" => None,
        ep => Some(ep.parse().unwrap()),
    };
    Pick {
        cands,
        tried: list(f[2]),
        want,
    }
}

#[derive(Default)]
struct Counts {
    splitmix: usize,
    rr_cases: usize,
    p2c_cases: usize,
    picks: usize,
    misses: usize,
}

#[test]
fn rust_policies_make_steers_picks() {
    let mut counts = Counts::default();
    let mut lines = FIXTURE.lines().enumerate().map(|(i, l)| (i + 1, l));
    while let Some((at, line)) = lines.next() {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.first().copied() {
            None | Some("#") => {}
            Some("u") => {
                let s = SplitMix64::new(f[1].parse().unwrap());
                for v in &f[2..] {
                    assert_eq!(
                        s.next_u64(),
                        u64::from_str_radix(v, 16).unwrap(),
                        "line {at}"
                    );
                }
                counts.splitmix += 1;
            }
            Some("i") => {
                let s = SplitMix64::new(f[1].parse().unwrap());
                let n: usize = f[2].parse().unwrap();
                for v in &f[3..] {
                    assert_eq!(s.below(n), v.parse::<usize>().unwrap(), "line {at}");
                }
                counts.splitmix += 1;
            }
            Some("rr") => {
                let weights: Vec<u32> = list(f[2]).into_iter().map(|w| w as u32).collect();
                let rr = RoundRobin::new(&weights);
                for (at, line) in lines.by_ref() {
                    let f: Vec<&str> = line.split_whitespace().collect();
                    if f == ["end"] {
                        break;
                    }
                    assert_eq!(f[0], "p", "line {at}: {line}");
                    let p = parse_pick(&f, weights.len());
                    let got = rr.pick(&p.cands, &p.tried, None);
                    assert_eq!(
                        got, p.want,
                        "line {at}: {line} (round robin, weights {weights:?})"
                    );
                    counts.picks += 1;
                    counts.misses += usize::from(got.is_none());
                }
                counts.rr_cases += 1;
            }
            Some("p2c") => {
                let seed: u64 = f[1].parse().unwrap();
                let n: usize = f[2].parse().unwrap();
                let clock = Arc::new(ManualClock::new());
                let eps: Arc<[Load]> = (0..n)
                    .map(|_| Load {
                        latency: PeakEwma::new(clock.start()),
                        in_flight: AtomicI64::new(0),
                    })
                    .collect();
                let ll = LeastLoaded::new(
                    Arc::clone(&eps),
                    Arc::new(SplitMix64::new(seed)),
                    clock.clone(),
                );
                for (at, line) in lines.by_ref() {
                    let f: Vec<&str> = line.split_whitespace().collect();
                    let num = |k: usize| -> u64 { f[k].parse().unwrap() };
                    match f[0] {
                        "end" => break,
                        "o" => eps[num(1) as usize]
                            .latency
                            .observe(Duration::from_nanos(num(2)), clock.now()),
                        "a" => {
                            eps[num(1) as usize]
                                .in_flight
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        "r" => {
                            eps[num(1) as usize]
                                .in_flight
                                .fetch_sub(1, Ordering::Relaxed);
                        }
                        "t" => {
                            clock.advance(Duration::from_nanos(num(1)));
                        }
                        "p" => {
                            let p = parse_pick(&f, n);
                            let got = ll.pick(&p.cands, &p.tried, None);
                            assert_eq!(got, p.want, "line {at}: {line} (p2c seed {seed})");
                            counts.picks += 1;
                            counts.misses += usize::from(got.is_none());
                        }
                        other => panic!("line {at}: unknown op {other}"),
                    }
                }
                counts.p2c_cases += 1;
            }
            Some(other) => panic!("line {at}: unknown record {other}"),
        }
    }
    // The whole fixture was read, and it exercises what it should.
    assert_eq!(counts.splitmix, 8);
    assert_eq!((counts.rr_cases, counts.p2c_cases), (40, 40));
    assert!(counts.picks > 14_000, "{} picks", counts.picks);
    assert!(counts.misses > 0, "no pick found every candidate tried");
}
