//! pacerd's config reader against edgefront-limiter's: every config in
//! legacy/pacer through both parsers, the golden parse-mode transcripts,
//! and the diff against drivers that answer wrongly or fail.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use edgefront_difftest::PACER_DIFFDRIVE;
use edgefront_difftest::pacer::DriverCommand;
use edgefront_difftest::pacer_config::{
    self, CORPUS_DIRS, Input, Plan, Source, c_answer, corpus, error_line, pacer_root, rust_answer,
};

const BIN: &str = env!("CARGO_BIN_EXE_edgefront-difftest");

/// The C driver behind a shell pipeline, such as a sed program that
/// rewrites its answer.
fn piped(filter: &str) -> DriverCommand {
    DriverCommand {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            format!("\"$0\" \"$@\" | {filter}"),
            PACER_DIFFDRIVE.into(),
        ],
    }
}

fn files_under(dir: &Path, out: &mut BTreeSet<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.insert(path);
        }
    }
}

#[test]
fn every_file_in_tests_examples_and_the_fuzz_corpus_agrees() {
    let root = pacer_root();
    let mut want = BTreeSet::new();
    for dir in CORPUS_DIRS {
        files_under(&root.join(dir), &mut want);
    }
    let inputs = corpus(&root).unwrap();
    let files: BTreeSet<PathBuf> = inputs
        .iter()
        .filter_map(|i| match &i.source {
            Source::File(p) => Some(p.clone()),
            Source::Bytes(_) => None,
        })
        .collect();
    assert_eq!(files, want, "the corpus must hold every file");
    assert!(files.len() >= 40, "{} files", files.len());

    let driver = DriverCommand::c_driver();
    let mut accepted = Vec::new();
    let mut lines = BTreeSet::new();
    for input in inputs
        .iter()
        .filter(|i| matches!(i.source, Source::File(_)))
    {
        let c = c_answer(&driver, input).unwrap();
        let rust = rust_answer(input);
        assert_eq!(c, rust, "{}", input.name);
        if c.starts_with("ok\n") {
            accepted.push(input.name.clone());
        } else {
            lines.insert(error_line(&c).expect("a file error names its line"));
        }
    }
    for name in [
        "examples/pacerd.conf",
        "fuzz/corpus/config/full.conf",
        "fuzz/corpus/config/minimal.conf",
        "fuzz/corpus/config/two.conf",
        "tests/diffdrive/config/full.conf",
        "tests/diffdrive/config/defaults.conf",
    ] {
        assert!(accepted.iter().any(|a| a == name), "{name} in {accepted:?}");
    }
    // C sources and op scripts are rejected, at more than one line.
    assert!(lines.len() > 1, "{lines:?}");
}

#[test]
fn the_literals_include_the_parser_tests_configs() {
    let inputs = corpus(&pacer_root()).unwrap();
    let full = inputs
        .iter()
        .find(|i| {
            i.name.starts_with("literal tests/test_config.c:")
                && !i.name.contains(',')
                && i.bytes()
                    .unwrap()
                    .starts_with(b"# pacerd example\nworkers 4\n")
        })
        .expect("config_full_example's text");
    let text = full.bytes().unwrap();
    assert!(text.ends_with(b"    upstream 10.0.0.2:22\n}"), "{text:?}");
    assert!(rust_answer(full).starts_with("ok\nworkers=4 "));
    // The integration tests' configs parse once their ports are filled in.
    let filled = inputs
        .iter()
        .filter(|i| i.name.starts_with("literal tests/it_") && i.name.ends_with(", filled in"))
        .filter(|i| rust_answer(i).starts_with("ok\n"))
        .count();
    assert!(filled >= 5, "{filled} integration configs parse");
    // Directives wrapped in expect_listener_error's block.
    assert!(inputs.iter().any(|i| {
        i.name.ends_with(", in a listener block")
            && rust_answer(i).starts_with("error line 4: buffer: ")
    }));
}

#[test]
fn the_whole_corpus_and_mutants_agree_with_c() {
    let plan = Plan {
        mutants: 3_000,
        seed: 20_261_107,
    };
    let report = pacer_config::run(&DriverCommand::c_driver(), &pacer_root(), plan).unwrap();
    assert!(report.divergences.is_empty(), "{report}");
    assert_eq!(report.mutants, 3_000);
    assert!(report.literals > 1_000, "{report}");
    assert_eq!(report.accepted + report.rejected, report.inputs());
    assert!(report.accepted > 300, "{report}");
    assert!(report.rejected > 2_000, "{report}");
    let text = report.to_string();
    assert!(text.starts_with("seed 20261107\ninputs: "), "{text}");
    assert!(text.ends_with("\ndivergences: 0"), "{text}");
}

#[test]
fn the_rust_side_prints_the_golden_parse_transcripts() {
    let dir = pacer_root().join("tests/diffdrive/config");
    let mut checked = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        let conf = entry.unwrap().path();
        if conf.extension().is_none_or(|e| e != "conf") {
            continue;
        }
        let want = fs::read_to_string(conf.with_extension("out")).unwrap();
        let file = Input {
            name: conf.display().to_string(),
            source: Source::File(conf.clone()),
        };
        let bytes = Input {
            name: file.name.clone(),
            source: Source::Bytes(fs::read(&conf).unwrap()),
        };
        assert_eq!(rust_answer(&file), want, "{}", conf.display());
        assert_eq!(rust_answer(&bytes), want, "{}", conf.display());
        let c = DriverCommand::c_driver();
        assert_eq!(c_answer(&c, &file).unwrap(), want, "{}", conf.display());
        assert_eq!(c_answer(&c, &bytes).unwrap(), want, "{}", conf.display());
        checked += 1;
    }
    assert_eq!(checked, 4);
}

#[test]
fn oversized_and_nul_inputs_agree() {
    let c = DriverCommand::c_driver();
    let mut big = b"workers 1\n".repeat((1 << 20) / 10);
    big.extend_from_slice(b"listener a {\n listen 127.0.0.1:1\n upstream 127.0.0.1:2\n}\n");
    let mut cases = vec![
        big.clone(),
        b"workers 1\nwork\0ers 2\n".to_vec(),
        Vec::new(),
        b"\n\n\n".to_vec(),
    ];
    big.truncate(1 << 20);
    cases.push(big);
    for (i, bytes) in cases.into_iter().enumerate() {
        let input = Input {
            name: format!("case {i}"),
            source: Source::Bytes(bytes),
        };
        assert_eq!(
            c_answer(&c, &input).unwrap(),
            rust_answer(&input),
            "case {i}"
        );
    }
}

#[test]
fn a_wrong_error_line_is_reported_with_both_lines() {
    let driver = piped("sed 's/^error line 4: /error line 5: /'");
    let plan = Plan {
        mutants: 400,
        seed: 77,
    };
    let report = pacer_config::run(&driver, &pacer_root(), plan).unwrap();
    assert!(!report.divergences.is_empty(), "{report}");
    for d in &report.divergences {
        assert_eq!(error_line(&d.c), Some(5), "{d}");
        assert_eq!(error_line(&d.rust), Some(4), "{d}");
        assert!(d.to_string().contains("error line: C 5, Rust 4\n"), "{d}");
    }
    let mutant = report
        .divergences
        .iter()
        .find(|d| d.seed.is_some())
        .expect("a mutant fails at line 4");
    let seed = mutant.seed.unwrap();
    assert!(mutant.to_string().contains(&format!(
        "replay: edgefront-difftest pacer-config-mutant --seed {seed}"
    )));

    // The replay command gives the same bytes, and the same two answers.
    let out = Command::new(BIN)
        .args(["pacer-config-mutant", "--seed", &seed.to_string()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let replay = Input {
        name: "replay".into(),
        source: Source::Bytes(out.stdout),
    };
    assert_eq!(c_answer(&driver, &replay).unwrap(), mutant.c);
    assert_eq!(rust_answer(&replay), mutant.rust);
    assert_eq!(
        c_answer(&DriverCommand::c_driver(), &replay).unwrap(),
        mutant.rust
    );
}

#[test]
fn a_changed_value_is_reported() {
    let driver = piped("sed 's/ table_size=4096 / table_size=4097 /'");
    let report = pacer_config::run(
        &driver,
        &pacer_root(),
        Plan {
            mutants: 0,
            seed: 1,
        },
    )
    .unwrap();
    let names: Vec<&str> = report.divergences.iter().map(|d| d.name.as_str()).collect();
    assert!(
        names.contains(&"tests/diffdrive/config/full.conf"),
        "{names:?}"
    );
    let d = &report.divergences[0];
    let text = d.to_string();
    assert!(text.contains("first differing answer line: 3\n"), "{text}");
    assert!(text.contains("error line: C -, Rust -\n"), "{text}");
}

#[test]
fn a_failing_or_silent_driver_is_an_error_not_a_divergence() {
    let root = pacer_root();
    let plan = Plan {
        mutants: 0,
        seed: 1,
    };
    for (driver, want) in [
        (piped("true"), "printed no answer"),
        (piped("cat; echo oops >&2"), "exited with"),
        (piped("cat; exit 3"), "exited with"),
        (
            DriverCommand {
                program: "/nonexistent/diffdrive".into(),
                args: Vec::new(),
            },
            "",
        ),
    ] {
        let err = pacer_config::run(&driver, &root, plan).unwrap_err();
        assert!(err.to_string().contains(want), "{err}");
    }
}

#[test]
fn the_cli_runs_replays_and_rejects_bad_options() {
    let out = Command::new(BIN)
        .args(["pacer-config", "--mutants", "200", "--seed", "5"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.starts_with("seed 5\ninputs: "), "{stdout}");
    assert!(stdout.contains(" 200 mutants)\n"), "{stdout}");
    assert!(stdout.contains("\ndivergences: 0\ntook "), "{stdout}");

    let out = Command::new(BIN)
        .args([
            "pacer-config",
            "--mutants",
            "0",
            "--seed",
            "5",
            "--",
            "sh",
            "-c",
        ])
        .arg(format!(
            "\"{PACER_DIFFDRIVE}\" \"$@\" | sed 's/workers=4 /workers=5 /'"
        ))
        .arg("sh")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.contains("divergence on tests/diffdrive/config/full.conf\n"),
        "{stdout}"
    );

    let mutant = |seed: &str| {
        Command::new(BIN)
            .args(["pacer-config-mutant", "--seed", seed])
            .output()
            .unwrap()
            .stdout
    };
    assert_eq!(mutant("12"), mutant("12"));
    assert_ne!(mutant("12"), mutant("13"));

    // The mutant goes to the driver unchanged.
    let mut child = Command::new(PACER_DIFFDRIVE)
        .arg("config")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&mutant("12"))
        .unwrap();
    let c = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    let rust = rust_answer(&Input {
        name: "12".into(),
        source: Source::Bytes(mutant("12")),
    });
    assert_eq!(c, rust);

    for args in [
        &["pacer-config", "--mutants"][..],
        &["pacer-config", "--mutants", "x"],
        &["pacer-config", "--bogus", "1"],
        &["pacer-config", "--"],
        &["pacer-config-mutant", "--mutants", "3"],
    ] {
        let out = Command::new(BIN).args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("usage:"),
            "{args:?}"
        );
    }

    let out = Command::new(BIN)
        .args([
            "pacer-config",
            "--mutants",
            "0",
            "--",
            "/nonexistent/diffdrive",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("(seed "));
}
