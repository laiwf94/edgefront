use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use proptest::prelude::*;

use super::*;

fn p(s: &str) -> Prefix {
    s.parse().unwrap()
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

fn class(name: &str, action: Action, prefixes: &[&str]) -> Class {
    let mut c = Class::new(name, action);
    c.prefixes = prefixes.iter().map(|s| p(s)).collect();
    c
}

fn name(c: &Classifier, a: &str) -> Option<String> {
    c.classify(ip(a)).map(|x| x.class.name.clone())
}

#[test]
fn the_most_specific_prefix_decides() {
    let c = Classifier::new(vec![
        class(
            "blocked",
            Action::Deny,
            &["203.0.113.0/24", "2001:db8::/32"],
        ),
        class(
            "partner",
            Action::Limit,
            &["203.0.113.128/25", "2001:db8:1::/48"],
        ),
        class("office", Action::Allow, &["203.0.113.200/32"]),
    ])
    .unwrap();
    assert_eq!(name(&c, "203.0.113.1").as_deref(), Some("blocked"));
    assert_eq!(name(&c, "203.0.113.130").as_deref(), Some("partner"));
    assert_eq!(name(&c, "203.0.113.200").as_deref(), Some("office"));
    assert_eq!(name(&c, "2001:db8:2::1").as_deref(), Some("blocked"));
    assert_eq!(name(&c, "2001:db8:1:5::1").as_deref(), Some("partner"));
    assert_eq!(name(&c, "198.51.100.1"), None);
    assert_eq!(name(&c, "::ffff:203.0.113.200").as_deref(), Some("office"));
    let x = c.classify(ip("203.0.113.130")).unwrap();
    assert_eq!(
        (x.index, x.prefix.to_string()),
        (1, "203.0.113.128/25".into())
    );
    assert_eq!(x.class.action, Action::Limit);
    assert_eq!(c.classes().len(), 3);
    assert_eq!(c.prefixes(), 5);
    assert!(c.memory() > 0);
    assert_eq!(Classifier::empty().classify(ip("1.2.3.4")), None);
    assert_eq!(Action::Deny.to_string(), "deny");
    assert_eq!(Action::Allow.as_str(), "allow");
}

#[test]
fn ambiguous_or_empty_classes_are_refused() {
    let e = Classifier::new(vec![
        class("a", Action::Deny, &["10.0.0.0/8"]),
        class("b", Action::Allow, &["10.0.0.0/8"]),
    ])
    .unwrap_err();
    assert_eq!(e, "10.0.0.0/8 is in classes a and b");
    // A class may repeat its own prefix.
    assert!(
        Classifier::new(vec![class(
            "a",
            Action::Deny,
            &["10.0.0.0/8", "10.0.0.0/8"]
        )])
        .is_ok()
    );
    let e = Classifier::new(vec![
        class("a", Action::Deny, &["10.0.0.0/8"]),
        class("a", Action::Allow, &["11.0.0.0/8"]),
    ])
    .unwrap_err();
    assert_eq!(e, "class a is defined twice");
    assert_eq!(
        Classifier::new(vec![class("a", Action::Deny, &[])]).unwrap_err(),
        "class a: no prefixes"
    );
    let mut bad = class("a", Action::Limit, &["::/0"]);
    bad.ipv6_prefix = 0;
    assert_eq!(
        Classifier::new(vec![bad]).unwrap_err(),
        "class a: ipv6_prefix must be 1 to 128, got 0"
    );
}

fn reference(classes: &[Class], a: IpAddr) -> Option<&str> {
    let mut best: Option<(u8, &str)> = None;
    for c in classes {
        for pr in &c.prefixes {
            if pr.contains(a) && best.is_none_or(|(l, _)| pr.len() > l) {
                best = Some((pr.len(), &c.name));
            }
        }
    }
    best.map(|(_, n)| n)
}

/// Done when, item 2: 10,000 random overlapping prefixes spread over
/// classes, every lookup equal to a linear scan for the most specific.
#[test]
fn ten_thousand_overlapping_prefixes_classify_as_a_linear_scan() {
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut classes: Vec<Class> = (0..20)
        .map(|i| {
            let action = [Action::Allow, Action::Deny, Action::Limit][i % 3];
            Class::new(format!("c{i}"), action)
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut total = 0;
    while total < 10_000 {
        let r = next();
        // Few distinct high bits, so prefixes nest and overlap.
        let pr = if r % 4 == 0 {
            let len = (r >> 8) as u8 % 129;
            let a = (u128::from((r >> 16) as u8 % 4) << 124) | (u128::from(next()) << 60);
            let a = if len == 0 {
                0
            } else {
                a & (u128::MAX << (128 - u32::from(len)))
            };
            Prefix::new(IpAddr::V6(Ipv6Addr::from(a)), len).unwrap()
        } else {
            let len = 4 + (r >> 8) as u8 % 29;
            let a = (0x0a00_0000 | ((r >> 32) as u32 & 0x00ff_ffff))
                & (u32::MAX << (32 - u32::from(len)));
            Prefix::new(IpAddr::V4(Ipv4Addr::from(a)), len).unwrap()
        };
        if seen.insert(pr) {
            classes[(r >> 40) as usize % 20].prefixes.push(pr);
            total += 1;
        }
    }
    let c = Classifier::new(classes.clone()).unwrap();
    assert_eq!(c.prefixes(), 10_000);
    for i in 0..20_000 {
        let r = next();
        let a = if i % 3 == 0 {
            IpAddr::V6(Ipv6Addr::from(
                (u128::from(r as u8 % 4) << 124) | (u128::from(next()) << 60) | u128::from(r),
            ))
        } else {
            IpAddr::V4(Ipv4Addr::from(0x0a00_0000 | (r as u32 & 0x00ff_ffff)))
        };
        assert_eq!(
            c.classify(a).map(|x| x.class.name.as_str()),
            reference(&classes, a),
            "{a}"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn classification_equals_a_linear_scan(
        raw in proptest::collection::vec((any::<u32>(), 8u8..=32, 0usize..4), 1..200),
        probes in proptest::collection::vec(any::<u32>(), 1..200),
    ) {
        let mut classes: Vec<Class> = (0..4).map(|i| Class::new(format!("c{i}"), Action::Limit)).collect();
        let mut owner = std::collections::HashMap::new();
        for (a, len, k) in raw {
            // Under 10.0.0.0/8, so they overlap.
            let a = (0x0a00_0000 | (a & 0x00ff_ffff)) & (u32::MAX << (32 - u32::from(len)));
            let pr = Prefix::new(IpAddr::V4(Ipv4Addr::from(a)), len).unwrap();
            let k = *owner.entry(pr).or_insert(k);
            classes[k].prefixes.push(pr);
        }
        classes.retain(|c| !c.prefixes.is_empty());
        let c = Classifier::new(classes.clone()).unwrap();
        for a in probes {
            let a = IpAddr::V4(Ipv4Addr::from(0x0a00_0000 | (a & 0x00ff_ffff)));
            prop_assert_eq!(c.classify(a).map(|x| x.class.name.as_str()), reference(&classes, a));
        }
    }
}
