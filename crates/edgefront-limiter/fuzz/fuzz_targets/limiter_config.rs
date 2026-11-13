//! Fuzzes the pacerd.conf reader, ported from legacy/pacer/fuzz/fuzz_config.c.
//!
//! Besides finding panics, it checks what pacer's target checked, and what
//! the port adds on top:
//!
//! - an accepted file gives a usable config: 1 to 16 listeners, 1 to 64
//!   workers, names of 1 to 31 bytes of letters, digits, `_` and `-`,
//!   buffers of at least 1 KiB, at least one connection, tables of at least
//!   16 keys, and rates the limiter takes;
//! - a rejected file gives one printable line, `line N: ...`, where N is a
//!   line of the input, or `config larger than ...` for a file over 1 MiB;
//! - an accepted config passes `validate`, writes out as pacerd.conf and as
//!   TOML, and each reads back into an equal config;
//! - lints never panic, and each names a line of the input and one
//!   printable line of text.
//!
//! CI runs it with `-timeout=10`, so a parse that loops or goes quadratic
//! on a crafted file fails as a timeout.

#![no_main]

use edgefront_limiter::config::{
    LimiterConfig, MAX_FILE_BYTES, MAX_LISTENERS, NAME_MAX, parse_pacerd, parse_pacerd_mapped,
};
use edgefront_limiter::lint::lint;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match parse_pacerd_mapped(data) {
        Ok((cfg, map)) => {
            check_usable(&cfg);
            check_round_trips(&cfg);
            let lines = line_count(data);
            assert_eq!(map.listeners.len(), cfg.listeners.len());
            for l in lint(&cfg, &map) {
                assert!(
                    (1..=lines).contains(&l.line),
                    "lint on line {} of a {lines}-line file: {l}",
                    l.line
                );
                assert!(printable(&l.to_string()), "lint text: {l:?}");
                assert!(cfg.listeners.iter().any(|c| c.name == l.listener));
            }
        }
        Err(e) => {
            let text = e.to_string();
            assert!(printable(&text), "error text: {text:?}");
            if data.len() > MAX_FILE_BYTES {
                assert_eq!(e.line, 0);
                assert!(e.message.starts_with("config larger"), "{text}");
            } else {
                // The empty file has no line 1: "no listener defined"
                // names line 0, as in pacer.
                let lines = line_count(data);
                assert!(e.line <= lines, "{text} in a {lines}-line file");
                assert!(e.line >= 1 || lines == 0, "{text}");
            }
            // The reader is a pure function of its input.
            assert_eq!(parse_pacerd(data), Err(e));
        }
    }
});

/// What pacer's `check_ok` held an accepted config to.
fn check_usable(cfg: &LimiterConfig) {
    assert!((1..=MAX_LISTENERS).contains(&cfg.listeners.len()));
    assert!((1..=64).contains(&cfg.workers));
    for l in &cfg.listeners {
        assert!(
            !l.name.is_empty() && l.name.len() < NAME_MAX,
            "{:?}",
            l.name
        );
        assert!(
            l.name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'),
            "{:?}",
            l.name
        );
        assert!(l.buffer_size >= 1024);
        assert!(l.max_conns >= 1);
        assert!(l.table_size >= 16);
        for rate in [&l.per_ip_conn_rate, &l.per_ip_bandwidth, &l.bandwidth]
            .into_iter()
            .flatten()
        {
            rate.ticks()
                .unwrap_or_else(|e| panic!("listener {}: rate {rate:?}: {e:?}", l.name));
        }
    }
    let mut names: Vec<_> = cfg.listeners.iter().map(|l| &l.name).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), cfg.listeners.len(), "duplicate listener names");
}

/// Both writers give text their reader takes back to the same config.
fn check_round_trips(cfg: &LimiterConfig) {
    cfg.validate()
        .unwrap_or_else(|e| panic!("an accepted config fails validate: {e}"));
    // The tokenizer splits on the bytes to_pacerd refuses in a path, so a
    // parsed config always writes out.
    let text = cfg
        .to_pacerd()
        .unwrap_or_else(|e| panic!("an accepted config does not write out: {e}"));
    let again = parse_pacerd(&text).unwrap_or_else(|e| {
        panic!(
            "written config is rejected: {e}\n{}",
            String::from_utf8_lossy(&text)
        )
    });
    assert_eq!(&again, cfg, "pacerd.conf round trip");
    // TOML needs UTF-8 paths; pacerd.conf does not.
    if cfg
        .admin_socket
        .as_ref()
        .is_some_and(|p| p.to_str().is_none())
    {
        return;
    }
    let toml_text =
        toml::to_string(cfg).unwrap_or_else(|e| panic!("an accepted config is not TOML: {e}"));
    let back: LimiterConfig = toml::from_str(&toml_text)
        .unwrap_or_else(|e| panic!("written TOML is rejected: {e}\n{toml_text}"));
    assert_eq!(&back, cfg, "TOML round trip");
}

/// Lines as the reader counts them: a final line need not end in `\n`.
fn line_count(data: &[u8]) -> usize {
    let newlines = data.iter().filter(|&&c| c == b'\n').count();
    newlines + usize::from(data.last().is_some_and(|&c| c != b'\n'))
}

fn printable(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| (0x20..0x7f).contains(&c))
}
