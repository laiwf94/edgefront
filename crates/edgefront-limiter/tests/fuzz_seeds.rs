//! Keeps the `limiter_config` fuzz target's seeds and dictionary in step
//! with the reader, so the fuzzer starts from configs that get past the
//! first line and knows every word the reader takes.

use std::fs;
use std::path::{Path, PathBuf};

use edgefront_limiter::config::{directive_names, parse_pacerd};

fn fuzz_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz")
}

/// The dictionary's entries, unescaped as libFuzzer reads them: one
/// `"..."` per line, `#` lines are comments, `\xNN` and `\\` escapes.
fn dictionary() -> Vec<Vec<u8>> {
    let text = fs::read_to_string(fuzz_dir().join("limiter_config.dict")).unwrap();
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let body = line
            .strip_prefix('"')
            .and_then(|l| l.strip_suffix('"'))
            .unwrap_or_else(|| panic!("dictionary line {}: {line:?} is not quoted", n + 1));
        let mut word = Vec::new();
        let mut bytes = body.bytes();
        while let Some(c) = bytes.next() {
            if c != b'\\' {
                word.push(c);
                continue;
            }
            match bytes.next() {
                Some(b'x') => {
                    let hex: Vec<u8> = bytes.by_ref().take(2).collect();
                    let hex = std::str::from_utf8(&hex).unwrap();
                    word.push(u8::from_str_radix(hex, 16).unwrap_or_else(|_| {
                        panic!("dictionary line {}: bad escape \\x{hex}", n + 1)
                    }));
                }
                Some(c @ (b'\\' | b'"')) => word.push(c),
                other => panic!("dictionary line {}: bad escape {other:?}", n + 1),
            }
        }
        assert!(!word.is_empty(), "dictionary line {}: empty entry", n + 1);
        out.push(word);
    }
    out
}

#[test]
fn dictionary_has_every_directive_and_keyword() {
    let dict = dictionary();
    let has = |w: &[u8]| dict.iter().any(|d| d == w);
    for name in directive_names() {
        assert!(
            has(name.as_bytes()),
            "{name} is missing from the dictionary"
        );
    }
    for word in [
        "listener", "{", "}", "per", "off", "burst", "deny", "evict", "debug", "info", "warn",
        "error", "/s", "/m", "/h", "#", "\n",
    ] {
        assert!(
            has(word.as_bytes()),
            "{word:?} is missing from the dictionary"
        );
    }
    let mut sorted = dict.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), dict.len(), "the dictionary repeats an entry");
}

#[test]
fn every_seed_is_an_accepted_config() {
    let dir = fuzz_dir().join("corpus/limiter_config");
    let mut seeds: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("seed-"))
        })
        .collect();
    seeds.sort();
    assert!(seeds.len() >= 3, "seeds in {}: {seeds:?}", dir.display());
    let mut listeners = 0;
    for seed in &seeds {
        let cfg = parse_pacerd(&fs::read(seed).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", seed.display()));
        listeners += cfg.listeners.len();
    }
    // Together the seeds reach past one listener per file.
    assert!(listeners > seeds.len(), "{listeners} listeners");
}
