//! Properties of the whole balancer over live endpoint state: outside panic
//! no pick ever lands on an endpoint that is unhealthy or ejected, every
//! level takes its share of traffic within one percentage point of Envoy's
//! priority load, and in panic round robin spreads over every endpoint by
//! weight.
//!
//! The reference is Envoy's priority math written out independently of
//! `priority.rs`: each level's health is `min(100, 140 × available / total)`
//! in integers, the total is normalised to at most 100, each level takes
//! `health × 100 / normalised total` of what is left, and the rounding
//! leftover goes to the first level with health. Panic is a normalised
//! total below the threshold.

use std::sync::Arc;
use std::time::Duration;

use edgefront_core::clock::{Clock, ManualClock};
use edgefront_core::config::Policy;
use edgefront_proxy::balancer::rng::SplitMix64;
use edgefront_proxy::balancer::{Balancer, Options};
use edgefront_proxy::endpoint::Endpoint;
use proptest::prelude::*;

const PICKS: usize = 100_000;

/// What an endpoint is doing when the picks start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Up,
    Unhealthy,
    /// Ejected until well after the picks.
    Ejected,
    /// Ejected, but the ejection ran out before the picks.
    Readmitted,
}

#[derive(Debug, Clone)]
struct Case {
    policy: Policy,
    panic_threshold: u8,
    /// `(priority, weight, status)` per endpoint.
    endpoints: Vec<(u8, u32, Status)>,
    seed: u64,
    /// Whether Maglev picks carry keys.
    keyed: bool,
}

fn status() -> impl Strategy<Value = Status> {
    prop_oneof![
        5 => Just(Status::Up),
        2 => Just(Status::Unhealthy),
        2 => Just(Status::Ejected),
        1 => Just(Status::Readmitted),
    ]
}

fn case() -> impl Strategy<Value = Case> {
    let policy = prop_oneof![
        Just(Policy::RoundRobin),
        Just(Policy::LeastLoaded),
        Just(Policy::Maglev),
    ];
    let threshold = prop_oneof![
        Just(0u8),
        Just(25),
        Just(50),
        Just(75),
        Just(100),
        0u8..=100
    ];
    // Up to six levels, with gaps: a priority with no endpoints is a level
    // with no health.
    let endpoints = prop::collection::vec((0u8..6, 1u32..=1000, status()), 1..=40);
    (policy, threshold, endpoints, any::<u64>(), any::<bool>()).prop_map(
        |(policy, panic_threshold, endpoints, seed, keyed)| Case {
            policy,
            panic_threshold,
            endpoints,
            seed,
            keyed,
        },
    )
}

/// Envoy's priority load for these per-level counts: `None` in panic,
/// `Some` of all zeros when nothing is available.
fn envoy_load(available: &[u32], total: &[u32], panic_threshold: u8) -> Option<Vec<u32>> {
    let health: Vec<u32> = available
        .iter()
        .zip(total)
        .map(|(&a, &t)| (140 * a).checked_div(t).map_or(0, |h| h.min(100)))
        .collect();
    let normalised: u32 = health.iter().sum::<u32>().min(100);
    if normalised < u32::from(panic_threshold) {
        return None;
    }
    let mut load = vec![0u32; health.len()];
    if normalised == 0 {
        return Some(load);
    }
    let mut remaining = 100u32;
    for (l, &h) in load.iter_mut().zip(&health) {
        *l = (h * 100 / normalised).min(remaining);
        remaining -= *l;
    }
    if let Some(first) = health.iter().position(|&h| h > 0) {
        load[first] += remaining;
    }
    Some(load)
}

fn check(case: &Case) -> Result<(), TestCaseError> {
    let clock = Arc::new(ManualClock::new());
    let epoch = clock.now();
    let eps: Vec<Endpoint> = case
        .endpoints
        .iter()
        .enumerate()
        .map(|(i, &(p, w, _))| Endpoint::new(&format!("10.1.0.{i}:80"), w, p.into(), epoch))
        .collect();
    for (e, &(_, _, s)) in eps.iter().zip(&case.endpoints) {
        match s {
            Status::Up => {}
            Status::Unhealthy => e.state.set_healthy(false),
            Status::Ejected => {
                e.state.eject(
                    clock.now(),
                    Duration::from_secs(3600),
                    Duration::from_secs(3600),
                    "test",
                );
            }
            Status::Readmitted => {
                e.state.eject(
                    clock.now(),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    "test",
                );
            }
        }
    }
    clock.advance(Duration::from_secs(5));
    let opts = Options::new(case.policy)
        .panic_threshold(case.panic_threshold)
        .clock(Arc::clone(&clock) as Arc<dyn Clock>)
        .rng(Arc::new(SplitMix64::new(case.seed)))
        .maglev_table_size(251);
    let lb = Balancer::new(eps, opts).expect("a valid balancer");

    let levels = lb.levels();
    let mut available = vec![0u32; levels];
    let mut total = vec![0u32; levels];
    let up: Vec<bool> = case
        .endpoints
        .iter()
        .map(|&(_, _, s)| matches!(s, Status::Up | Status::Readmitted))
        .collect();
    for (&(p, _, _), &u) in case.endpoints.iter().zip(&up) {
        total[usize::from(p)] += 1;
        available[usize::from(p)] += u32::from(u);
    }
    let reference = envoy_load(&available, &total, case.panic_threshold);

    let keys = SplitMix64::new(case.seed ^ 0x9e37_79b9_7f4a_7c15);
    let mut per_endpoint = vec![0usize; case.endpoints.len()];
    let mut per_level = vec![0usize; levels];
    let mut none = 0;
    for _ in 0..PICKS {
        let key = (case.policy == Policy::Maglev && case.keyed).then(|| keys.next_u64());
        match lb.pick(key, &[]) {
            Some(i) => {
                per_endpoint[i] += 1;
                per_level[usize::from(case.endpoints[i].0)] += 1;
            }
            None => none += 1,
        }
    }

    match reference {
        Some(load) if load.iter().all(|&l| l == 0) => {
            prop_assert_eq!(none, PICKS, "nothing is available, yet there were picks");
        }
        Some(load) => {
            prop_assert_eq!(none, 0, "available endpoints, yet no pick");
            for (i, &n) in per_endpoint.iter().enumerate() {
                prop_assert!(
                    n == 0 || up[i],
                    "endpoint {} ({:?}) took {} picks outside panic",
                    i,
                    case.endpoints[i].2,
                    n
                );
            }
            for (p, (&n, &want)) in per_level.iter().zip(&load).enumerate() {
                let share = n as f64 * 100.0 / PICKS as f64;
                prop_assert!(
                    (share - f64::from(want)).abs() <= 1.0,
                    "level {}: {:.2}% of picks, Envoy's load {}% (available {:?} of {:?})",
                    p,
                    share,
                    want,
                    available,
                    total
                );
            }
        }
        None => {
            // In panic every endpoint is a candidate, whatever its state.
            prop_assert_eq!(none, 0, "no pick in panic");
            if case.policy == Policy::RoundRobin {
                let weights: u32 = case.endpoints.iter().map(|e| e.1).sum();
                for (i, &n) in per_endpoint.iter().enumerate() {
                    let share = n as f64 * 100.0 / PICKS as f64;
                    let want = f64::from(case.endpoints[i].1) * 100.0 / f64::from(weights);
                    prop_assert!(
                        (share - want).abs() <= 1.0,
                        "endpoint {} in panic: {:.2}%, weight share {:.2}%",
                        i,
                        share,
                        want
                    );
                }
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn picks_respect_availability_and_envoys_priority_load(case in case()) {
        check(&case)?;
    }
}

/// The reference itself, on Envoy's documented examples.
#[test]
fn the_reference_matches_envoys_examples() {
    // Two levels, both healthy: all of it to P0.
    assert_eq!(envoy_load(&[5, 5], &[5, 5], 0), Some(vec![100, 0]));
    // P0 at 50%: 70% there, 30% to P1.
    assert_eq!(envoy_load(&[2, 4], &[4, 4], 0), Some(vec![70, 30]));
    // P0 at 20%, P1 at 20%: normalised 56, so 50/50.
    assert_eq!(envoy_load(&[1, 1], &[5, 5], 0), Some(vec![50, 50]));
    // The same under a 60% panic threshold.
    assert_eq!(envoy_load(&[1, 1], &[5, 5], 60), None);
    // Nothing available.
    assert_eq!(envoy_load(&[0, 0], &[3, 0], 0), Some(vec![0, 0]));
    // A level with no endpoints in between, and the last level capped by
    // what is left.
    assert_eq!(envoy_load(&[1, 0, 2], &[3, 0, 3], 0), Some(vec![46, 0, 54]));
    // A rounding leftover goes to the first level with health.
    assert_eq!(
        envoy_load(&[0, 1, 1, 1], &[2, 5, 5, 5], 0),
        Some(vec![0, 34, 33, 33])
    );
}
