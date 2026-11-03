//! Builds legacy/pacer's differential driver, `tests/diffdrive.c` linked
//! with libpacer's sources, into OUT_DIR with the system C compiler (`CC`,
//! or `cc` when unset), and hands its path to the crate as
//! `EDGEFRONT_PACER_DIFFDRIVE`. The driver runs as a child process that
//! reads operations on stdin, so no FFI or unsafe code is involved.
//!
//! The warnings and sanitizer builds of the same sources belong to
//! legacy/pacer's own gate; this build only needs a working binary.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let pacer = manifest.join("../../legacy/pacer");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("pacer-diffdrive");

    let mut sources = vec![pacer.join("tests/diffdrive.c")];
    let mut lib: Vec<PathBuf> = fs::read_dir(pacer.join("src/lib"))
        .unwrap_or_else(|e| panic!("reading {}: {e}", pacer.join("src/lib").display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "c"))
        .collect();
    lib.sort();
    sources.extend(lib);

    println!("cargo::rerun-if-env-changed=CC");
    println!(
        "cargo::rerun-if-changed={}",
        pacer.join("include").display()
    );
    println!(
        "cargo::rerun-if-changed={}",
        pacer.join("src/lib").display()
    );
    for source in &sources {
        println!("cargo::rerun-if-changed={}", source.display());
    }

    let cc = env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let mut cmd = Command::new(&cc);
    cmd.args(["-std=c17", "-O2", "-pthread", "-D_GNU_SOURCE", "-w"])
        .arg(include(&pacer, "include"))
        .arg(include(&pacer, "src"))
        .args(&sources)
        .arg("-o")
        .arg(&out);
    let status = cmd.status().unwrap_or_else(|e| {
        panic!("running the C compiler `{cc}` (set CC to choose another): {e}")
    });
    assert!(
        status.success(),
        "building the pacer diffdrive failed: {cmd:?} exited with {status}"
    );

    println!(
        "cargo::rustc-env=EDGEFRONT_PACER_DIFFDRIVE={}",
        out.display()
    );
}

fn include(root: &Path, dir: &str) -> String {
    format!("-I{}", root.join(dir).display())
}
