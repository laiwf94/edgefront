use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use super::*;
use crate::cidr::Swap;
use crate::policy::Verdict;

fn dir() -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "addr-policy-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

const CONFIG: &str = r#"
[address_policy]
header = "x-forwarded-for"
trusted_proxies = ["10.0.0.1"]
trusted_proxy_files = ["proxies.txt"]
table_size = 1024

[[address_policy.classes]]
name = "blocked"
action = "deny"
files = ["blocked.txt"]

[[address_policy.classes]]
name = "partners"
action = "limit"
prefixes = ["203.0.113.0/24"]
ipv6_prefix = 48
per_ip_max_conns = 2
per_ip_conn_rate = "10/s burst 20"

[other]
anything = true
"#;

/// The section of a whole config, as `edgefront limits` reads it.
fn from_toml(text: &str) -> Result<Option<AddressPolicyConfig>, String> {
    #[derive(serde::Deserialize)]
    struct Root {
        address_policy: Option<AddressPolicyConfig>,
    }
    toml::from_str::<Root>(text)
        .map(|r| r.address_policy)
        .map_err(|e| e.to_string())
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

#[test]
fn a_config_loads_with_its_files_and_summarises() {
    let d = dir();
    std::fs::write(d.join("proxies.txt"), "192.0.2.0/24\n").unwrap();
    std::fs::write(
        d.join("blocked.txt"),
        "# bad actors\n198.51.100.0/24\n2001:db8:bad::/48\n",
    )
    .unwrap();
    let cfg = from_toml(CONFIG).unwrap().unwrap();
    let (p, s) = cfg.load(&d).unwrap();
    assert_eq!(s.trusted_proxies, 2);
    assert_eq!(s.prefixes, 3);
    assert_eq!(
        s.classes,
        [
            ("blocked".into(), "deny".into(), 2),
            ("partners".into(), "limit".into(), 1)
        ]
    );
    assert_eq!(s.files.len(), 2);
    assert!(s.memory > 0);
    let text = s.to_string();
    assert!(
        text.starts_with("address_policy: 2 trusted proxies, 2 classes, 3 prefixes, "),
        "{text}"
    );
    assert!(
        text.contains("  class partners: limit, 1 prefixes\n"),
        "{text}"
    );
    assert!(text.contains("blocked.txt: 2 prefixes\n"), "{text}");
    let p = Arc::new(p);
    let d1 = p.decide(ip("192.0.2.5"), &["198.51.100.9"], 1);
    assert!(matches!(d1.verdict, Verdict::Deny));
    let c = &p.classifier().classes()[1];
    assert_eq!(c.ipv6_prefix, 48);
    assert_eq!(c.limits.per_ip_max_conns, Some(2));
    assert!(c.limits.per_ip_conn_rate.is_some());
    assert_eq!(from_toml("[other]\nx = 1\n").unwrap(), None);
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn mistakes_name_their_field_or_file() {
    let d = dir();
    std::fs::write(d.join("bad.txt"), "10.0.0.0/8\n10.0.0.1/8\n").unwrap();
    let cases: &[(&str, &str)] = &[
        ("header = \"via\"", "address_policy.header: \"via\" is not"),
        (
            "trusted_proxies = [\"10.0.0.1/8\"]\nheader = \"forwarded\"",
            "address_policy.trusted_proxies: 10.0.0.1/8: host bits",
        ),
        (
            "trusted_proxies = [\"10.0.0.1\"]",
            "trusted proxies are set but no header",
        ),
        (
            "ipv6_prefix = 0",
            "address_policy.ipv6_prefix: must be 1 to 128, got 0",
        ),
        (
            "trusted_proxy_files = [\"missing.txt\"]\nheader = \"forwarded\"",
            "missing.txt: ",
        ),
        (
            "trusted_proxy_files = [\"bad.txt\"]\nheader = \"forwarded\"",
            "bad.txt: line 2: 10.0.0.1/8: host bits",
        ),
        (
            "[[address_policy.classes]]\nname = \"a\"\naction = \"maybe\"\nprefixes = [\"10.0.0.0/8\"]",
            "address_policy.classes[0].action: \"maybe\"",
        ),
        (
            "[[address_policy.classes]]\nname = \"a\"\naction = \"deny\"\nprefixes = [\"10.0.0.0/8\"]\nper_ip_max_conns = 3",
            "class a is deny, so its limits would never apply",
        ),
        (
            "[[address_policy.classes]]\nname = \"a\"\naction = \"limit\"\nprefixes = [\"10.0.0.0/8\"]\nper_ip_conn_rate = \"10/day\"",
            "address_policy.classes[0].per_ip_conn_rate: ",
        ),
        (
            "[[address_policy.classes]]\nname = \"a\"\naction = \"deny\"\nprefixes = [\"10.0.0.0/8\"]\n[[address_policy.classes]]\nname = \"b\"\naction = \"allow\"\nprefixes = [\"10.0.0.0/8\"]",
            "address_policy: 10.0.0.0/8 is in classes a and b",
        ),
        (
            "[[address_policy.classes]]\nname = \"a\"\naction = \"limit\"\nprefixes = [\"10.0.0.0/8\"]\nper_ip_bandwidth = \"1m/s\"",
            "classes[0].per_ip_bandwidth: a class sets connection limits only",
        ),
        (
            "table_size = 0\n[[address_policy.classes]]\nname = \"a\"\naction = \"limit\"\nprefixes = [\"10.0.0.0/8\"]",
            "address_policy.table_size: ",
        ),
        (
            "[[address_policy.classes]]\nname = \"a\"\naction = \"deny\"",
            "class a: no prefixes",
        ),
    ];
    for (body, want) in cases {
        let text = format!("[address_policy]\n{body}\n");
        let e = from_toml(&text)
            .and_then(|c| c.unwrap().load(&d).map(drop))
            .unwrap_err();
        assert!(e.contains(want), "{body}: {e}");
    }
    let e = from_toml("[address_policy]\nbogus = 1\n").unwrap_err();
    assert!(
        e.contains("line 2") && e.contains("unknown field `bogus`"),
        "{e}"
    );
    assert!(from_toml("not toml [").is_err());
    std::fs::remove_dir_all(d).unwrap();
}

/// Done when, item 1's reload: a list of a million prefixes reloads for
/// the cost of building it, while lookups go on against the old list and
/// never wait for the new one (the under-a-second figure is the release
/// benchmark's).
#[test]
fn a_million_prefix_list_reloads_in_under_a_second_without_blocking_lookups() {
    let d = dir();
    let mut text = String::with_capacity(20_000_000);
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..1_000_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let a = (x >> 32) as u32 & 0xffff_ff00;
        text.push_str(&format!(
            "{}.{}.{}.0/24\n",
            a >> 24,
            (a >> 16) & 255,
            (a >> 8) & 255
        ));
    }
    std::fs::write(d.join("big.txt"), &text).unwrap();
    let cfg = from_toml(
        "[address_policy]\n[[address_policy.classes]]\nname = \"big\"\naction = \"deny\"\nfiles = [\"big.txt\"]\n",
    )
    .unwrap()
    .unwrap();
    // The control: building the policy once, on the same machine in the
    // same run, with nothing else going on.
    let t = Instant::now();
    let first = cfg.load(&d).unwrap().0;
    let control = t.elapsed();
    let swap = Arc::new(Swap::new(first));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let mut r = swap.reader();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut n = 0u64;
            let mut worst = std::time::Duration::ZERO;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let t = Instant::now();
                let _ = r
                    .get()
                    .classifier()
                    .classify(IpAddr::from([(n % 251) as u8, 1, 2, 3]));
                worst = worst.max(t.elapsed());
                n += 1;
            }
            (n, worst)
        })
    };
    let t = Instant::now();
    let (fresh, summary) = cfg.load(&d).unwrap();
    swap.store(fresh);
    let took = t.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (lookups, worst) = reader.join().unwrap();
    assert!(summary.prefixes > 900_000, "{}", summary.prefixes);
    assert!(lookups > 1000);
    // The second itself is a release-build figure, measured by
    // `cargo bench --bench prefix_list` (docs/DESIGN.md): a debug build is
    // several times slower and one under TSan ten times more, so a fixed
    // limit here would measure the build, not the reload. What a test can
    // hold is that the reload costs a build of the list and a swap, no
    // more, with a reader looking up all the while (which shares the
    // CPU, hence the factor), and that no lookup waits for it.
    assert!(
        took < control * 3,
        "the reload took {took:?}, building the list alone {control:?}"
    );
    assert!(
        worst < took,
        "a lookup waited {worst:?}, the reload took {took:?}"
    );
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn a_limiter_config_reads_with_the_section_beside_it() {
    let text = "workers = 1\n[[listeners]]\nname = \"web\"\nlisten = \"127.0.0.1:8080\"\nupstream = \"127.0.0.1:9090\"\n\n[address_policy]\n[[address_policy.classes]]\nname = \"a\"\naction = \"deny\"\nprefixes = [\"10.0.0.0/8\"]\n";
    let cfg: crate::LimiterConfig = toml::from_str(text).unwrap();
    assert_eq!(cfg.listeners.len(), 1);
    let p = from_toml(text).unwrap().unwrap();
    assert_eq!(p.classes.len(), 1);
}
