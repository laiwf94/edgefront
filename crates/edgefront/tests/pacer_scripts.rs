//! pacer's integration scenarios (`legacy/pacer/tests/it_*.c`) as the
//! traffic scripts in `tests/pacer_scripts`, run against `edgefront
//! limits`, and against legacy pacerd too when `PACERD_BIN` and
//! `PACERCTL_BIN` name its binaries (`make difftest-l4`, which needs
//! Linux). Every step must meet its expectation on every binary, and the
//! two transcripts must agree line by line.
//!
//! Two scripts connect from loopback addresses other than 127.0.0.1,
//! which only Linux has without configuration; on other hosts they are
//! named and left to the Linux CI jobs, which run all of them.

use std::path::{Path, PathBuf};

use edgefront_difftest::l4::{self, Target};

#[test]
fn pacers_scenarios_pass_against_every_binary_with_the_same_outcomes() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/pacer_scripts");
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
    let mut targets = Vec::new();
    if let (Some(d), Some(c)) = (
        std::env::var_os("PACERD_BIN"),
        std::env::var_os("PACERCTL_BIN"),
    ) {
        targets.push(Target::Pacerd {
            pacerd: PathBuf::from(d),
            pacerctl: PathBuf::from(c),
        });
    }
    targets.push(Target::Edgefront {
        bin: PathBuf::from(env!("CARGO_BIN_EXE_edgefront")),
    });
    let work = std::env::temp_dir().join(format!("pacer-scripts-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let reports = l4::run_all(&targets, &scripts, &work);
    let _ = std::fs::remove_dir_all(&work);
    let mut failed = 0;
    for r in &reports {
        eprint!("{r}");
        failed += usize::from(!r.ok());
    }
    assert_eq!(failed, 0, "{failed} of {} scripts failed", reports.len());
}
