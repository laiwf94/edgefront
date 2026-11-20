//! Shadow limits count exactly what enforcing copies refuse, and a
//! recording replayed against its own policy changes nothing.
//!
//! Requests are generated from a seed: 1,000 keys under a skewed choice,
//! times moving forward by a random step, a limit of 20 a second with a
//! burst of 10, so keys run from never refused to mostly refused. The
//! same requests go through a limiter whose rule is in shadow mode and
//! one whose identical rule enforces, request by request.

use std::sync::Arc;

use edgefront_core::filter::Decision;
use edgefront_limiter::clock::ManualClock;
use edgefront_limiter::config::http_limits::{HttpLimitsSection, build};
use edgefront_limiter::http::HttpLimiter;
use edgefront_limiter::record::{RecordConfig, Recorder, read};
use edgefront_limiter::replay::replay;
use edgefront_limiter::shadow::Mode;
use http::{HeaderMap, HeaderValue, Method, Uri};
use proptest::prelude::*;

fn rules(mode: &str) -> String {
    format!(
        "[[http_limits]]\nname = \"api\"\nrate = \"20/s burst 10\"\nkey = [\"header:x-key\"]\nmode = \"{mode}\"\n"
    )
}

fn limiter(text: &str, clock: Arc<ManualClock>) -> Arc<HttpLimiter> {
    let s: HttpLimitsSection = toml::from_str(text).unwrap();
    Arc::new(HttpLimiter::new(build(&s.http_limits).unwrap(), clock).unwrap())
}

struct Gen(u64);

impl Gen {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Runs `n` generated requests; returns the enforcing copy's refusals
/// and the shadow rule's would-block count, after checking each request.
fn run(seed: u64, n: usize) -> (u64, u64) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let clock = Arc::new(ManualClock::new(1_000_000_000));
    let shadow = limiter(&rules("shadow"), clock.clone());
    let enforce = limiter(&rules("enforce"), clock.clone());
    let mut g = Gen(seed | 1);
    let uri: Uri = "/".parse().unwrap();
    let mut refused = 0u64;
    for _ in 0..n {
        clock.advance(g.next() % 200_000);
        // Skewed: low keys far more often.
        let r = g.next() % 1_000_000;
        let k = (r * r / 1_000_000_000) % 1000;
        let mut h = HeaderMap::new();
        h.insert("x-key", HeaderValue::from(k));
        let s = rt.block_on(shadow.decide(&[0], &Method::GET, &uri, &h));
        let e = rt.block_on(enforce.decide(&[0], &Method::GET, &uri, &h));
        assert!(
            !matches!(s, Decision::Respond(_)),
            "a shadow rule never refuses"
        );
        refused += u64::from(matches!(e, Decision::Respond(_)));
        assert_eq!(shadow.shadow_stats()[0].2, refused, "request by request");
    }
    (refused, shadow.shadow_stats()[0].2)
}

#[test]
fn a_million_requests_shadow_counts_equal_enforcing_refusals() {
    let (refused, would) = run(42, 1_000_000);
    assert_eq!(refused, would);
    assert!(refused > 10_000 && refused < 900_000, "{refused}");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]
    #[test]
    fn shadow_equals_enforce_for_any_seed(seed in any::<u64>()) {
        let (refused, would) = run(seed, 20_000);
        prop_assert_eq!(refused, would);
    }
}

#[test]
fn a_recording_replayed_against_its_own_policy_changes_nothing_and_promotion_keeps_state() {
    let dir = std::env::temp_dir().join(format!("replay-eq-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let clock = Arc::new(ManualClock::new(1_000_000_000));
    let l = limiter(&rules("shadow"), clock.clone());
    let rec = Arc::new(
        Recorder::new(
            RecordConfig {
                dir: dir.clone(),
                ring: 1 << 20,
                file_bytes: 1 << 20,
                files: 100,
            },
            l.rule_names(),
        )
        .unwrap(),
    );
    let l = Arc::new(
        HttpLimiter::new(
            build(
                &toml::from_str::<HttpLimitsSection>(&rules("shadow"))
                    .unwrap()
                    .http_limits,
            )
            .unwrap(),
            clock.clone(),
        )
        .unwrap()
        .with_recorder(Arc::clone(&rec)),
    );
    let mut g = Gen(7);
    let uri: Uri = "/".parse().unwrap();
    for _ in 0..100_000 {
        clock.advance(g.next() % 100_000);
        let mut h = HeaderMap::new();
        h.insert("x-key", HeaderValue::from(g.next() % 300));
        rt.block_on(l.decide(&[0], &Method::GET, &uri, &h));
    }
    rec.flush().unwrap();
    let recording = read(&dir).unwrap();
    assert_eq!(recording.records.len(), 100_000);
    let same = build(
        &toml::from_str::<HttpLimitsSection>(&rules("enforce"))
            .unwrap()
            .http_limits,
    )
    .unwrap();
    let r = replay(&recording, &same).unwrap();
    assert_eq!(r.rules[0].refused_before, l.shadow_stats()[0].2);
    assert_eq!(
        (r.rules[0].newly_refused, r.rules[0].newly_admitted),
        (0, 0)
    );
    assert!(r.keys.is_empty());
    // A tighter candidate lists the keys it changes.
    let tight = build(
        &toml::from_str::<HttpLimitsSection>(
            &rules("enforce").replace("20/s burst 10", "5/s burst 5"),
        )
        .unwrap()
        .http_limits,
    )
    .unwrap();
    let r = replay(&recording, &tight).unwrap();
    assert!(r.rules[0].newly_refused > 0 && !r.keys.is_empty());
    // Promotion: the same rules with the mode changed, applied in place.
    let mut h = HeaderMap::new();
    h.insert("x-key", HeaderValue::from_static("hot"));
    for _ in 0..10 {
        rt.block_on(l.decide(&[0], &Method::GET, &uri, &h));
    }
    assert!(matches!(
        rt.block_on(l.decide(&[0], &Method::GET, &uri, &h)),
        Decision::Forward(_)
    ));
    assert_eq!(
        l.apply_modes(&same).unwrap(),
        [("api".to_owned(), Mode::Enforce)]
    );
    // The key's bucket was spent while shadowed: refused at once.
    assert!(matches!(
        rt.block_on(l.decide(&[0], &Method::GET, &uri, &h)),
        Decision::Respond(_)
    ));
    assert!(
        l.apply_modes(&tight)
            .unwrap_err()
            .contains("changed more than its mode")
    );
    assert!(l.apply_modes(&[]).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}
