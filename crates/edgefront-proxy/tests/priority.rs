//! Properties of the priority load math, from steer's
//! `TestPriorityLoadProperties`, plus the share each level takes of
//! uniformly drawn rolls.

use edgefront_proxy::balancer::priority::{Census, Decision, LEVELS, priority_load};
use proptest::prelude::*;

/// Up to four levels of up to five endpoints each, some healthy, as steer's
/// randomised test draws them.
fn counts() -> impl Strategy<Value = (Vec<u32>, Vec<u32>)> {
    prop::collection::vec((0u32..6).prop_flat_map(|t| (0..=t, Just(t))), 1..5)
        .prop_map(|levels| levels.into_iter().unzip())
}

/// Any number of levels with any number of endpoints.
fn wide_counts() -> impl Strategy<Value = (Vec<u32>, Vec<u32>)> {
    prop::collection::vec((0u32..2000).prop_flat_map(|t| (0..=t, Just(t))), 0..=LEVELS)
        .prop_map(|levels| levels.into_iter().unzip())
}

/// Envoy's health per level with real numbers, floored as the integer math
/// floors it.
fn reference_health(healthy: &[u32], total: &[u32]) -> Vec<f64> {
    healthy
        .iter()
        .zip(total)
        .map(|(&h, &t)| {
            if t == 0 {
                0.0
            } else {
                (140.0 * f64::from(h) / f64::from(t)).floor().min(100.0)
            }
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20_000))]

    // TestPriorityLoadProperties: loads always sum to 100, are never
    // negative, follow priority order, and agree with a floating-point
    // reference within rounding.
    #[test]
    fn loads_agree_with_the_reference((healthy, total) in counts()) {
        let levels = total.len();
        let decision = priority_load(&healthy, &total, 0);
        prop_assert_ne!(decision, Decision::Panic, "panic with threshold 0");
        let health = reference_health(&healthy, &total);
        let sum: f64 = health.iter().sum();
        if sum == 0.0 {
            prop_assert_eq!(decision, Decision::Unavailable);
            return Ok(());
        }
        let Decision::Spread(loads) = decision else {
            return Err(TestCaseError::fail(format!("no loads for {healthy:?} of {total:?}")));
        };
        let load = loads.as_slice();
        prop_assert_eq!(load.len(), levels);
        let norm = sum.min(100.0);
        let mut remaining = 100.0;
        let mut got = 0u32;
        for p in 0..levels {
            let want = (health[p] * 100.0 / norm).floor().min(remaining);
            remaining -= want;
            prop_assert!(
                health[p] != 0.0 || load[p] == 0,
                "level {} has no health but load {:?} (healthy {:?} total {:?})",
                p, load, healthy, total
            );
            let d = f64::from(load[p]) - want;
            prop_assert!(
                (0.0..=levels as f64).contains(&d),
                "level {}: load {}, reference {} (healthy {:?} total {:?})",
                p, load[p], want, healthy, total
            );
            got += u32::from(load[p]);
        }
        prop_assert_eq!(got, 100, "loads {:?}", load);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    // Each level takes exactly its load of the 100 possible rolls, so with
    // uniform rolls its share of traffic is its load. Only the first level
    // with health takes more than the floored reference, and by less than
    // one point per level.
    #[test]
    fn rolls_split_by_load((healthy, total) in wide_counts(), threshold in 0u8..=100) {
        let decision = priority_load(&healthy, &total, threshold);
        let health = reference_health(&healthy, &total);
        let norm = health.iter().sum::<f64>().min(100.0);
        if norm < f64::from(threshold) {
            prop_assert_eq!(decision, Decision::Panic);
            return Ok(());
        }
        let Some(loads) = decision.loads() else {
            prop_assert_eq!(norm, 0.0);
            prop_assert_eq!(decision, Decision::Unavailable);
            return Ok(());
        };
        let mut taken = vec![0u8; loads.levels()];
        for roll in 0..100 {
            taken[loads.level_for(roll)] += 1;
        }
        prop_assert_eq!(&taken[..], loads.as_slice());
        let first = health.iter().position(|&h| h > 0.0).unwrap();
        let mut remaining = 100.0;
        for (p, &l) in loads.as_slice().iter().enumerate() {
            let want = (health[p] * 100.0 / norm).floor().min(remaining);
            remaining -= want;
            let extra = f64::from(l) - want;
            if p == first {
                prop_assert!((0.0..loads.levels() as f64).contains(&extra));
            } else {
                prop_assert_eq!(extra, 0.0);
            }
        }
    }

    // A census of endpoints decides exactly as the counts it gathered.
    #[test]
    fn census_matches_the_counts(
        endpoints in prop::collection::vec((0u8..LEVELS as u8, any::<bool>()), 0..200),
        threshold in 0u8..=100,
    ) {
        let census: Census = endpoints.iter().copied().collect();
        let levels = endpoints.iter().map(|&(p, _)| usize::from(p) + 1).max().unwrap_or(0);
        let mut healthy = vec![0; levels];
        let mut total = vec![0; levels];
        for &(p, available) in &endpoints {
            total[usize::from(p)] += 1;
            healthy[usize::from(p)] += u32::from(available);
        }
        prop_assert_eq!(census.healthy(), &healthy[..]);
        prop_assert_eq!(census.total(), &total[..]);
        let decision = census.decide(threshold);
        prop_assert_eq!(decision, priority_load(&healthy, &total, threshold));
        let state = census.state(threshold);
        prop_assert_eq!(state.panic, decision == Decision::Panic);
        prop_assert_eq!(state.priority_load.len(), levels);
    }
}
