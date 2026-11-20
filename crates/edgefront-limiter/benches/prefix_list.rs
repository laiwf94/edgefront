//! Longest-prefix match on a list of a million prefixes, and how long the
//! list takes to build: done-when item 1 of week 5 (under 100 ns at p99,
//! and a reload under a second).
//!
//! The list mixes IPv4 prefixes of /8 to /32 with IPv6 prefixes of /16
//! to /64 in a few top-level blocks, nine IPv4 to one IPv6, from a fixed
//! seed. 2,000,000 lookups of random addresses are timed in batches of
//! 32 dependent lookups (see the comment at the timing loop for why), and
//! the percentiles are of the per-lookup time of each batch. Lookups go
//! through a `Swap` reader, as the data path makes them.
//!
//! Run with `cargo bench -p edgefront-limiter --bench prefix_list`.

use std::hint::black_box;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use edgefront_limiter::cidr::{PrefixList, Swap};

const PREFIXES: usize = 1_000_000;
const LOOKUPS: usize = 2_000_000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn masked4(a: u32, len: u8) -> u32 {
    if len == 0 {
        0
    } else {
        a & (u32::MAX << (32 - u32::from(len)))
    }
}

fn masked6(a: u128, len: u8) -> u128 {
    if len == 0 {
        0
    } else {
        a & (u128::MAX << (128 - u32::from(len)))
    }
}

/// A list file of exactly [`PREFIXES`] distinct prefixes.
fn text(rng: &mut Rng) -> String {
    let mut seen = std::collections::HashSet::with_capacity(PREFIXES);
    let mut out = String::with_capacity(PREFIXES * 20);
    let mut i = 0usize;
    while seen.len() < PREFIXES {
        let r = rng.next();
        let line = if i % 10 == 9 {
            let len = 16 + (r % 49) as u8;
            let a =
                (0x2001_0db8u128 + u128::from((r >> 8) % 4)) << 96 | u128::from(rng.next()) << 32;
            format!("{}/{len}", Ipv6Addr::from(masked6(a, len)))
        } else {
            let len = 8 + (r % 25) as u8;
            format!("{}/{len}", Ipv4Addr::from(masked4((r >> 32) as u32, len)))
        };
        i += 1;
        if seen.insert(line.clone()) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

fn main() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let text = text(&mut rng);
    let t = Instant::now();
    let list = PrefixList::parse(&text).expect("generated prefixes parse");
    assert_eq!(list.len(), PREFIXES);
    let build = t.elapsed();
    println!(
        "prefix_list: {} prefixes parsed and built in {:.1} ms, {:.1} MiB",
        list.len(),
        build.as_secs_f64() * 1e3,
        list.memory() as f64 / (1 << 20) as f64
    );
    let swap = Arc::new(Swap::new(list));
    let mut reader = swap.reader();
    let addrs: Vec<IpAddr> = (0..LOOKUPS)
        .map(|i| {
            let r = rng.next();
            if i % 10 == 9 {
                IpAddr::V6(Ipv6Addr::from(
                    (0x2001_0db8u128 + u128::from(r % 4)) << 96 | u128::from(rng.next()),
                ))
            } else {
                IpAddr::V4(Ipv4Addr::from(r as u32))
            }
        })
        .collect();
    // macOS's clock ticks every 41.7 ns and reading it costs about as
    // much as a lookup, so single lookups cannot be timed honestly. Each
    // sample is a batch of BATCH lookups, each address depending on the
    // last answer so they cannot overlap (latency, not throughput), and
    // the sample is the batch's time over BATCH.
    const BATCH: usize = 32;
    let mut samples = Vec::with_capacity(LOOKUPS / BATCH);
    let mut found = 0usize;
    let mut carry = 0u32;
    let all = Instant::now();
    for chunk in addrs.as_chunks::<BATCH>().0 {
        let t = Instant::now();
        for a in chunk {
            let a = match *a {
                IpAddr::V4(v) => IpAddr::V4(Ipv4Addr::from(u32::from(v) ^ (carry & 1))),
                IpAddr::V6(v) => IpAddr::V6(Ipv6Addr::from(u128::from(v) ^ u128::from(carry & 1))),
            };
            let m = reader.get().lookup(black_box(a));
            carry = m.map_or(0, |m| m.prefix.len() as u32);
            found += usize::from(m.is_some());
        }
        samples.push(t.elapsed().as_nanos() as f64 / BATCH as f64);
    }
    let total = all.elapsed();
    samples.sort_unstable_by(f64::total_cmp);
    let pct = |s: &[f64], q: f64| s[((s.len() as f64 - 1.0) * q) as usize];
    println!(
        "prefix_list: dependent: {LOOKUPS} lookups in batches of {BATCH}, {found} matched; per lookup p50 {:.1} ns, p90 {:.1} ns, p99 {:.1} ns, p99.9 {:.1} ns",
        pct(&samples, 0.5),
        pct(&samples, 0.9),
        pct(&samples, 0.99),
        pct(&samples, 0.999),
    );
    // Independent: the same batches with no dependency, so the CPU
    // overlaps the batch's memory reads, as it does for lookups among the
    // data path's other work.
    let mut ind = Vec::with_capacity(LOOKUPS / BATCH);
    for chunk in addrs.as_chunks::<BATCH>().0 {
        let t = Instant::now();
        for a in chunk {
            black_box(reader.get().lookup(black_box(*a)));
        }
        ind.push(t.elapsed().as_nanos() as f64 / BATCH as f64);
    }
    ind.sort_unstable_by(f64::total_cmp);
    println!(
        "prefix_list: independent: {LOOKUPS} lookups in batches of {BATCH}; per lookup p50 {:.1} ns, p90 {:.1} ns, p99 {:.1} ns, p99.9 {:.1} ns",
        pct(&ind, 0.5),
        pct(&ind, 0.9),
        pct(&ind, 0.99),
        pct(&ind, 0.999),
    );
    // Untimed throughput, for the mean.
    let t = Instant::now();
    let mut hits = 0usize;
    for a in &addrs {
        hits += usize::from(reader.get().lookup(*a).is_some());
    }
    let untimed = t.elapsed();
    black_box(hits);
    println!(
        "prefix_list: mean {:.1} ns per lookup untimed ({:.1} dependent, timed)",
        untimed.as_nanos() as f64 / LOOKUPS as f64,
        total.as_nanos() as f64 / LOOKUPS as f64
    );
    // A reload while a reader keeps looking up.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let h = {
        let mut r = swap.reader();
        let stop = Arc::clone(&stop);
        let addrs = addrs[..10_000].to_vec();
        std::thread::spawn(move || {
            let mut worst = Duration::ZERO;
            let mut i = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let t = Instant::now();
                black_box(r.get().lookup(addrs[i % addrs.len()]));
                worst = worst.max(t.elapsed());
                i += 1;
            }
            (i, worst)
        })
    };
    let t = Instant::now();
    let fresh = PrefixList::parse(&text).expect("parses");
    swap.store(fresh);
    let reload = t.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (n, worst) = h.join().expect("reader");
    println!(
        "prefix_list: reload of {PREFIXES} prefixes in {:.1} ms; a reader made {n} lookups meanwhile, the slowest {} ns",
        reload.as_secs_f64() * 1e3,
        worst.as_nanos()
    );
}
