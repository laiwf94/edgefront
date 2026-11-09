//! The `cache` and `cache-record` commands.
//!
//! ```text
//! edgefront-difftest cache [--log FILE | --requests N --seed N] [--faketime LIB] [--dir DIR] [--keep N] -- REFERENCE CANDIDATE
//! edgefront-difftest cache-record [--requests N] [--seed N]
//! ```

use std::io::Write;
use std::path::PathBuf;

use super::{Plan, log, node};

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Replay (`cache`) or only print the generated log (`cache-record`).
    pub replay: bool,
    pub log: Option<PathBuf>,
    pub requests: usize,
    pub seed: u64,
    pub faketime: PathBuf,
    pub dir: PathBuf,
    pub keep: usize,
    /// REFERENCE and CANDIDATE.
    pub binaries: Vec<PathBuf>,
}

/// Parses the arguments after the command name. `seed` is used when there
/// is no `--seed`.
pub fn parse(args: &[String], replay: bool, seed: u64) -> Result<Command, String> {
    let mut cmd = Command {
        replay,
        log: None,
        requests: 50_000,
        seed,
        faketime: node::DEFAULT_FAKETIME.into(),
        dir: std::env::temp_dir().join(format!("edgefront-difftest-cache-{}", std::process::id())),
        keep: 20,
        binaries: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--" && replay {
            cmd.binaries = it.map(PathBuf::from).collect();
            if cmd.binaries.len() != 2 {
                return Err("expected -- REFERENCE CANDIDATE".into());
            }
            return Ok(cmd);
        }
        let value = it.next().ok_or_else(|| format!("{arg} needs a value"))?;
        let number = |v: &str| {
            v.parse::<u64>()
                .map_err(|_| format!("{arg}: {v:?} is not a number"))
        };
        match arg.as_str() {
            "--requests" => cmd.requests = number(value)? as usize,
            "--seed" => cmd.seed = number(value)?,
            "--log" if replay => cmd.log = Some(value.into()),
            "--faketime" if replay => cmd.faketime = value.into(),
            "--dir" if replay => cmd.dir = value.into(),
            "--keep" if replay => cmd.keep = number(value)? as usize,
            _ => return Err(format!("unknown option {arg}")),
        }
    }
    if replay {
        return Err("missing -- REFERENCE CANDIDATE".into());
    }
    Ok(cmd)
}

/// Runs a parsed command and returns its exit code: 0 when the nodes
/// agreed on every request, 1 when they did not, 2 when the run could not
/// be made.
pub fn run(cmd: &Command, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    if !cmd.replay {
        let text = log::write(&log::generate(cmd.seed, cmd.requests));
        return match out.write_all(text.as_bytes()) {
            Ok(()) => 0,
            Err(e) => {
                let _ = writeln!(err, "edgefront-difftest: {e}");
                2
            }
        };
    }
    let (entries, source) = match &cmd.log {
        Some(path) => {
            let parsed = std::fs::read_to_string(path)
                .map_err(|e| e.to_string())
                .and_then(|text| log::parse(&text));
            match parsed {
                Ok(entries) => (entries, format!("log {}", path.display())),
                Err(e) => {
                    let _ = writeln!(err, "edgefront-difftest: {}: {e}", path.display());
                    return 2;
                }
            }
        }
        None => (
            log::generate(cmd.seed, cmd.requests),
            format!(
                "{} generated requests, seed {} (replay with --seed {})",
                cmd.requests, cmd.seed, cmd.seed
            ),
        ),
    };
    let _ = writeln!(out, "cache: {source}");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a tokio runtime starts");
    let plan = Plan {
        entries: &entries,
        reference: &cmd.binaries[0],
        candidate: &cmd.binaries[1],
        faketime: &cmd.faketime,
        dir: &cmd.dir,
        keep: cmd.keep,
    };
    let result = runtime.block_on(super::run(&plan));
    let _ = std::fs::remove_dir_all(&cmd.dir);
    match result {
        Ok(report) => {
            let _ = write!(out, "{}", report.render());
            if report.divergent == 0 {
                0
            } else {
                let _ = writeln!(out, "cache: {source}");
                1
            }
        }
        Err(e) => {
            let _ = writeln!(err, "edgefront-difftest: cache: {e} ({source})");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_replay_and_record() {
        let c = parse(&args(&["--", "a", "b"]), true, 9).unwrap();
        assert_eq!(c.seed, 9);
        assert_eq!(c.requests, 50_000);
        assert_eq!(c.keep, 20);
        assert_eq!(c.faketime, PathBuf::from(node::DEFAULT_FAKETIME));
        assert_eq!(c.binaries, [PathBuf::from("a"), PathBuf::from("b")]);
        let c = parse(
            &args(&[
                "--requests",
                "5",
                "--seed",
                "3",
                "--log",
                "l",
                "--faketime",
                "f",
                "--dir",
                "d",
                "--keep",
                "1",
                "--",
                "a",
                "b",
            ]),
            true,
            0,
        )
        .unwrap();
        assert_eq!((c.requests, c.seed, c.keep), (5, 3, 1));
        assert_eq!(c.log, Some("l".into()));
        assert_eq!(c.faketime, PathBuf::from("f"));
        assert_eq!(c.dir, PathBuf::from("d"));
        let r = parse(&args(&["--requests", "2", "--seed", "4"]), false, 0).unwrap();
        assert!(!r.replay);
        assert_eq!((r.requests, r.seed), (2, 4));
    }

    #[test]
    fn refuses_bad_command_lines() {
        for (a, replay, want) in [
            (&["--"][..], true, "expected -- REFERENCE CANDIDATE"),
            (&["--", "a"][..], true, "expected -- REFERENCE CANDIDATE"),
            (&[][..], true, "missing -- REFERENCE CANDIDATE"),
            (&["--seed"][..], true, "--seed needs a value"),
            (&["--seed", "x"][..], true, "--seed: \"x\" is not a number"),
            (&["--log", "f"][..], false, "unknown option --log"),
            (&["--", "a", "b"][..], false, "unknown option --"),
            (&["--what", "1"][..], true, "unknown option --what"),
        ] {
            assert_eq!(parse(&args(a), replay, 0).unwrap_err(), want, "{a:?}");
        }
    }

    #[test]
    fn record_prints_the_generated_log() {
        let cmd = parse(&args(&["--requests", "50", "--seed", "5"]), false, 0).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run(&cmd, &mut out, &mut err), 0);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(log::parse(&text).unwrap(), log::generate(5, 50));
        assert!(err.is_empty());
    }

    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("closed"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn record_reports_a_write_error() {
        let cmd = parse(&args(&["--requests", "1"]), false, 0).unwrap();
        let mut err = Vec::new();
        assert_eq!(run(&cmd, &mut Broken, &mut err), 2);
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "edgefront-difftest: closed\n"
        );
    }

    #[test]
    fn replay_reports_a_bad_log_and_a_bad_setup() {
        let dir = std::env::temp_dir().join(format!("difftest-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.jsonl");
        std::fs::write(&bad, "{\"t\":1}\n").unwrap();
        let a = |extra: &[&str]| {
            let mut v = args(extra);
            v.extend(args(&["--dir", dir.join("run").to_str().unwrap()]));
            v.extend(args(&["--", "/nonexistent/a", "/nonexistent/b"]));
            v
        };
        let cmd = parse(&a(&["--log", bad.to_str().unwrap()]), true, 0).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run(&cmd, &mut out, &mut err), 2);
        let e = String::from_utf8(err).unwrap();
        assert!(e.contains("bad.jsonl: line 1: missing field"), "{e}");

        let cmd = parse(&a(&["--log", "/nonexistent/log"]), true, 0).unwrap();
        let mut err = Vec::new();
        assert_eq!(run(&cmd, &mut Vec::new(), &mut err), 2);
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("/nonexistent/log: ")
        );

        let cmd = parse(
            &a(&[
                "--requests",
                "10",
                "--seed",
                "1",
                "--faketime",
                "/nonexistent/ft.so",
            ]),
            true,
            0,
        )
        .unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run(&cmd, &mut out, &mut err), 2);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "cache: 10 generated requests, seed 1 (replay with --seed 1)\n"
        );
        let e = String::from_utf8(err).unwrap();
        assert!(
            e.contains("libfaketime not found at /nonexistent/ft.so"),
            "{e}"
        );
        assert!(
            e.ends_with("(10 generated requests, seed 1 (replay with --seed 1))\n"),
            "{e}"
        );
        assert!(!dir.join("run").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
