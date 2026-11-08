//! Maglev against steer, table byte for table byte.
//!
//! `fixtures/steer_maglev.txt` was written by `TestMaglevVectors` in
//! `legacy/steer/cmd/diffdrive`, which builds steer's own `balancer.Maglev`
//! over 50 endpoint sets (the first two of 100 endpoints at the default
//! 65,537 slots) and records `HashKey` vectors, every lookup table it
//! populates and a run of keyed and keyless picks; that test also fails if
//! steer would now write anything else. Here each set is rebuilt with the
//! Rust Maglev. Every table must hold the same bytes as steer's in memory
//! (one little-endian `int32` per slot, compared through both the slot
//! listing and an FNV-1a digest of those bytes), and every pick must match.

use std::sync::Arc;

use edgefront_proxy::balancer::Policy;
use edgefront_proxy::balancer::maglev::{self, DEFAULT_TABLE_SIZE, Endpoint, Maglev};
use edgefront_proxy::balancer::rng::SplitMix64;

const FIXTURE: &str = include_str!("fixtures/steer_maglev.txt");

fn list(s: &str, n: usize) -> Vec<usize> {
    match s {
        "-" => Vec::new(),
        "*" => (0..n).collect(),
        _ => s.split(',').map(|x| x.parse().unwrap()).collect(),
    }
}

fn unhex(s: &str) -> Vec<u8> {
    assert_eq!(s.len() % 2, 0, "odd hex {s:.40}");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// The table as steer holds it: one little-endian int32 per slot.
fn table_bytes(entries: &[i32]) -> Vec<u8> {
    entries.iter().flat_map(|e| e.to_le_bytes()).collect()
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[derive(Default, Debug)]
struct Counts {
    hashes: usize,
    sets: usize,
    full_size: usize,
    tables: usize,
    slots: usize,
    keyed: usize,
    keyless: usize,
    retries: usize,
    misses: usize,
}

#[test]
fn rust_maglev_builds_steers_tables_and_picks() {
    let mut c = Counts::default();
    let mut lines = FIXTURE.lines().enumerate().map(|(i, l)| (i + 1, l));
    while let Some((at, line)) = lines.next() {
        let f: Vec<&str> = line.split(' ').collect();
        match f[0] {
            "#" | "" => {}
            "h" => {
                let key = if f[1] == "-" { Vec::new() } else { unhex(f[1]) };
                let want = u64::from_str_radix(f[2], 16).unwrap();
                assert_eq!(maglev::hash_key(&key), want, "line {at}: HashKey");
                c.hashes += 1;
            }
            "maglev" => {
                let seed: u64 = f[1].parse().unwrap();
                let size: usize = f[2].parse().unwrap();
                let n: usize = f[3].parse().unwrap();
                let eps: Vec<Endpoint> = (0..n)
                    .map(|_| {
                        let (_, l) = lines.next().unwrap();
                        let e: Vec<&str> = l.split(' ').collect();
                        assert_eq!(e[0], "e", "line {at}: endpoint line {l}");
                        Endpoint::new(e[1], e[2].parse().unwrap())
                    })
                    .collect();
                let m = Maglev::new(&eps, size, Arc::new(SplitMix64::new(seed))).unwrap();
                c.sets += 1;
                c.full_size += usize::from(size == DEFAULT_TABLE_SIZE && n == 100);
                replay(&m, n, &mut lines, &mut c, at);
            }
            other => panic!("line {at}: unknown record {other:?}"),
        }
    }
    eprintln!("{c:?}");
    assert_eq!(c.hashes, 7);
    assert_eq!(c.sets, 50);
    assert_eq!(c.full_size, 2, "the two 100-endpoint, 65,537-slot sets");
    assert!(c.tables >= 140, "{} tables", c.tables);
    assert!(c.slots > 200_000, "{} slots", c.slots);
    assert_eq!(c.keyed + c.keyless, 50 * 60);
    assert!(c.keyless > 300 && c.retries > 500 && c.misses > 50, "{c:?}");
}

fn replay<'a>(
    m: &Maglev,
    n: usize,
    lines: &mut impl Iterator<Item = (usize, &'a str)>,
    c: &mut Counts,
    set_at: usize,
) {
    for (at, line) in lines.by_ref() {
        let f: Vec<&str> = line.split(' ').collect();
        match f[0] {
            "t" => {
                let cands = list(f[1], n);
                let digest = u64::from_str_radix(f[2], 16).unwrap();
                let slots = unhex(f[3]);
                let got = m.populate(&cands);
                assert_eq!(got.len(), slots.len(), "line {at}: table size");
                if let Some(s) = (0..slots.len()).find(|&s| got[s] as u8 != slots[s]) {
                    panic!(
                        "line {at} (set at line {set_at}): slot {s} is {} in Rust, {} in steer",
                        got[s], slots[s] as i8
                    );
                }
                assert_eq!(
                    fnv1a(&table_bytes(&got)),
                    digest,
                    "line {at}: table bytes differ from steer's"
                );
                c.tables += 1;
                c.slots += got.len();
            }
            "p" => {
                assert_eq!((f.len(), f[4]), (6, "->"), "line {at}: pick {line}");
                let cands = list(f[1], n);
                let tried = list(f[2], n);
                let key = (f[3] != "-").then(|| u64::from_str_radix(f[3], 16).unwrap());
                let want = match f[5] {
                    "-1" => None,
                    ep => Some(ep.parse::<usize>().unwrap()),
                };
                let got = m.pick(&cands, &tried, key);
                assert_eq!(
                    got, want,
                    "line {at} (set at line {set_at}): cands {} tried {} key {}",
                    f[1], f[2], f[3]
                );
                if key.is_some() {
                    c.keyed += 1;
                } else {
                    c.keyless += 1;
                }
                c.retries += usize::from(!tried.is_empty());
                c.misses += usize::from(want.is_none());
            }
            "end" => return,
            other => panic!("line {at}: unexpected {other:?} inside a set"),
        }
    }
    panic!("set at line {set_at} has no end");
}
