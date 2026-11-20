//! pacer's integration scenarios (`it_*.c`, from legacy/pacer) as the
//! traffic scripts in `tests/pacer_scripts`, run against `edgefront
//! limits`. Every step must meet its expectation, and the transcript must
//! equal, line for line, the one legacy pacerd gave for the same script
//! (`NAME.pacerd` beside it), recorded before legacy/pacer was deleted in
//! week 4. Until then `make difftest-l4` ran pacerd live beside edgefront.
//!
//! Two scripts connect from loopback addresses other than 127.0.0.1,
//! which only Linux has without configuration; on other hosts they are
//! named and left to the Linux CI jobs, which run all of them.

use std::path::{Path, PathBuf};

use edgefront_difftest::l4::{self, Target};

fn scripts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/pacer_scripts")
}

#[test]
fn pacers_scenarios_give_the_outcomes_pacerd_gave() {
    let dir = scripts_dir();
    let mut scripts = l4::load_dir(&dir).unwrap();
    assert_eq!(scripts.len(), 23, "one script per it_*.c scenario");
    if !l4::has_loopback_range() {
        let (linux, rest): (Vec<_>, Vec<_>) = scripts
            .into_iter()
            .partition(|(_, s)| s.needs_loopback_range);
        for (f, _) in &linux {
            eprintln!("{f}: needs 127.0.0.2 and up, run by the Linux CI jobs");
        }
        assert_eq!(linux.len(), 2);
        scripts = rest;
    }
    let target = Target::Edgefront {
        bin: PathBuf::from(env!("CARGO_BIN_EXE_edgefront")),
    };
    let work = std::env::temp_dir().join(format!("pacer-scripts-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let mut failed = Vec::new();
    for (file, script) in &scripts {
        let recorded = std::fs::read_to_string(dir.join(file.replace(".toml", ".pacerd")))
            .unwrap_or_else(|e| panic!("{file}: no recorded pacerd transcript: {e}"));
        let pacerd: Vec<String> = recorded.lines().map(str::to_owned).collect();
        let run = l4::run(&target, script, &work).unwrap_or_else(|e| panic!("{file}: {e}"));
        let divergence = l4::diff("pacerd", &pacerd, "edgefront", &run.transcript);
        if run.failed.is_empty() && divergence.is_none() {
            eprintln!("{file}: ok");
            continue;
        }
        eprintln!("{file}: FAILED");
        for f in &run.failed {
            eprintln!("  {f}");
        }
        if let Some(d) = divergence {
            eprintln!("  divergence at {d}");
        }
        for l in run
            .log
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .iter()
            .rev()
        {
            eprintln!("    | {l}");
        }
        failed.push(file.clone());
    }
    let _ = std::fs::remove_dir_all(&work);
    assert!(failed.is_empty(), "{failed:?}");
}

#[test]
fn every_script_has_its_recorded_transcript_and_nothing_else_is_there() {
    let dir = scripts_dir();
    let mut tomls = Vec::new();
    let mut recorded = Vec::new();
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        let stem = p.file_stem().unwrap().to_string_lossy().into_owned();
        match p.extension().and_then(|x| x.to_str()) {
            Some("toml") => tomls.push(stem),
            Some("pacerd") => recorded.push(stem),
            other => panic!("unexpected file {} ({other:?})", p.display()),
        }
    }
    tomls.sort();
    recorded.sort();
    assert_eq!(tomls, recorded);
    // A recorded transcript has one line per step of its script.
    for (file, script) in l4::load_dir(&dir).unwrap() {
        let text = std::fs::read_to_string(dir.join(file.replace(".toml", ".pacerd"))).unwrap();
        assert_eq!(text.lines().count(), script.step.len(), "{file}");
    }
}
