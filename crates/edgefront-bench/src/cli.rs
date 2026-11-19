//! The `edgefront-bench` command line.
//!
//! ```text
//! edgefront-bench run --scenario FILE [--scenario FILE]... --targets a,b [options]
//!     Runs each scenario against each target and writes
//!     <out>/<scenario>/<target>.json, then <out>/report.md.
//!     --targets-dir DIR   default bench/targets
//!     --out DIR           default target/bench/<run>
//!     --run NAME          default <date>-<first scenario>
//!     --target-cpus N     CPUs per target, default 4
//!     --machine TEXT      how the report names the machine
//!     --harness-image IMG default edgefront-bench/harness:local
//!     --rebuild           rebuild the images built from this repository
//! edgefront-bench report --results DIR --out FILE [--title TEXT] [--intro FILE]
//!     Renders every result under DIR into one Markdown report.
//! edgefront-bench validate FILE...
//!     Checks result files against bench/schema/result.schema.json.
//! edgefront-bench origin [--listen ADDR]
//!     Serves /obj/<size>/<name>, default on 0.0.0.0:9000.
//! edgefront-bench load --scenario FILE --target HOST:PORT --origin HOST:PORT
//!                      [--cgroup-root DIR] [--cgroup ID]...
//!     The load generator's side of a run, inside the network; prints JSON.
//! ```
//!
//! Exit codes: 0 on success, 2 for bad usage, 1 when the work fails.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use crate::report::{self, BenchResult};
use crate::scenario::Scenario;
use crate::targets::{self, Docker, LoadOptions, Runner, RunnerConfig, Target};

/// The usage text.
pub const USAGE: &str = "usage: edgefront-bench run --scenario FILE... --targets NAME,... [--targets-dir DIR] [--out DIR] [--run NAME] [--target-cpus N] [--machine TEXT] [--harness-image IMAGE] [--rebuild]
       edgefront-bench report --results DIR --out FILE [--title TEXT] [--intro FILE]
       edgefront-bench validate FILE...
       edgefront-bench origin [--listen ADDR]
       edgefront-bench load --scenario FILE --target HOST:PORT --origin HOST:PORT [--cgroup-root DIR] [--cgroup ID]...
";

/// Why a command failed.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// The arguments are wrong: exit 2.
    Usage(String),
    /// The work failed: exit 1.
    Failed(String),
}

impl Failure {
    /// The exit code.
    pub fn code(&self) -> ExitCode {
        match self {
            Failure::Usage(_) => ExitCode::from(2),
            Failure::Failed(_) => ExitCode::from(1),
        }
    }
}

fn usage(message: impl Into<String>) -> Failure {
    Failure::Usage(message.into())
}

fn failed(message: impl Into<String>) -> Failure {
    Failure::Failed(message.into())
}

/// The flags of one command: `--name value` pairs, `--switch`es and
/// positional arguments.
#[derive(Debug, Default)]
struct Flags {
    values: Vec<(String, String)>,
    switches: Vec<String>,
    positional: Vec<String>,
}

impl Flags {
    fn parse(args: &[String], valued: &[&str], switches: &[&str]) -> Result<Flags, Failure> {
        let mut flags = Flags::default();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if let Some(name) = arg.strip_prefix("--") {
                if switches.contains(&name) {
                    flags.switches.push(name.to_owned());
                } else if valued.contains(&name) {
                    let value = args
                        .get(i + 1)
                        .ok_or_else(|| usage(format!("--{name} needs a value")))?;
                    flags.values.push((name.to_owned(), value.clone()));
                    i += 1;
                } else {
                    return Err(usage(format!("unknown flag --{name}")));
                }
            } else {
                flags.positional.push(arg.clone());
            }
            i += 1;
        }
        Ok(flags)
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.values
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    fn one(&self, name: &str) -> Result<Option<&str>, Failure> {
        match self.all(name).as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(one)),
            _ => Err(usage(format!("--{name} is given more than once"))),
        }
    }

    fn required(&self, name: &str) -> Result<&str, Failure> {
        self.one(name)?
            .ok_or_else(|| usage(format!("--{name} is required")))
    }

    fn switch(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s == name)
    }

    fn no_positional(&self) -> Result<(), Failure> {
        match self.positional.first() {
            Some(extra) => Err(usage(format!("unexpected argument {extra:?}"))),
            None => Ok(()),
        }
    }
}

/// Runs the command line `args` (without the program name).
pub fn run(args: &[String]) -> Result<(), Failure> {
    let (command, rest) = args.split_first().ok_or_else(|| usage("no command"))?;
    match command.as_str() {
        "run" => run_targets(rest),
        "report" => render_report(rest),
        "validate" => validate(rest),
        "origin" => serve_origin(rest),
        "load" => load(rest),
        "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(usage(format!("unknown command {other:?}"))),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, Failure> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| failed(format!("cannot start the runtime: {e}")))
}

fn read_scenario(path: &str) -> Result<Scenario, Failure> {
    Scenario::load(path).map_err(|e| failed(e.to_string()))
}

/// The repository root: the nearest directory above `start` holding
/// `bench/targets`.
fn find_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join("bench/targets").is_dir())
        .map(Path::to_path_buf)
}

fn run_targets(args: &[String]) -> Result<(), Failure> {
    let flags = Flags::parse(
        args,
        &[
            "scenario",
            "targets",
            "targets-dir",
            "out",
            "run",
            "target-cpus",
            "machine",
            "harness-image",
        ],
        &["rebuild"],
    )?;
    flags.no_positional()?;
    let scenario_paths = flags.all("scenario");
    if scenario_paths.is_empty() {
        return Err(usage("--scenario is required"));
    }
    let names: Vec<&str> = flags
        .required("targets")?
        .split(',')
        .filter(|n| !n.is_empty())
        .collect();
    if names.is_empty() {
        return Err(usage("--targets names no target"));
    }
    let cwd = std::env::current_dir().map_err(|e| failed(e.to_string()))?;
    let root = find_root(&cwd).ok_or_else(|| {
        failed("run from inside the edgefront repository (no bench/targets above here)")
    })?;
    let targets_dir = flags
        .one("targets-dir")?
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("bench/targets"));
    let target_cpus = match flags.one("target-cpus")? {
        Some(n) => n.parse::<usize>().ok().filter(|n| *n > 0).ok_or_else(|| {
            usage(format!(
                "--target-cpus {n:?} must be a whole number above 0"
            ))
        })?,
        None => 4,
    };
    let scenarios: Vec<(Scenario, PathBuf)> = scenario_paths
        .iter()
        .map(|p| read_scenario(p).map(|s| (s, PathBuf::from(p))))
        .collect::<Result<_, _>>()?;
    let all = Target::load_all(&targets_dir).map_err(failed)?;
    let mut targets = Vec::new();
    for name in &names {
        let target = all.iter().find(|t| t.name == *name).ok_or_else(|| {
            let known: Vec<&str> = all.iter().map(|t| t.name.as_str()).collect();
            usage(format!(
                "no target {name:?} in {} (there are: {})",
                targets_dir.display(),
                known.join(", ")
            ))
        })?;
        targets.push(target.clone());
    }
    let today = &report::rfc3339(SystemTime::now())[..10];
    let run_name = flags
        .one("run")?
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{today}-{}", scenarios[0].0.name));
    let out = flags
        .one("out")?
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target/bench").join(&run_name));
    let command = ["edgefront-bench".to_owned(), "run".to_owned()]
        .into_iter()
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    let config = RunnerConfig {
        root: root.clone(),
        target_cpus,
        harness_image: flags
            .one("harness-image")?
            .unwrap_or(targets::HARNESS_IMAGE)
            .to_owned(),
        run: run_name.clone(),
        out: out.clone(),
        machine: flags.one("machine")?.unwrap_or("").to_owned(),
        command,
        local_tag: targets::local_tag(&root),
        lock_wait: Duration::from_secs(3 * 3600),
        lock_poll: Duration::from_secs(5),
    };
    let mut runner = Runner::new(Docker, config).map_err(failed)?;
    eprintln!("edgefront-bench: preparing images");
    runner
        .prepare(&targets, flags.switch("rebuild"))
        .map_err(failed)?;
    let mut results = Vec::new();
    for (scenario, path) in &scenarios {
        for target in &targets {
            eprintln!(
                "edgefront-bench: {} against {} ({:.0} s)",
                scenario.name,
                target.name,
                scenario.schedule().duration().as_secs_f64()
            );
            let result = runner.run(scenario, path, target).map_err(failed)?;
            eprintln!("{}", summary(&result));
            results.push(result);
        }
    }
    let text = report::render(&format!("Benchmark run {run_name}"), "", &results);
    let file = out.join("report.md");
    std::fs::write(&file, text).map_err(|e| failed(format!("{}: {e}", file.display())))?;
    eprintln!(
        "edgefront-bench: wrote {} results and {}",
        results.len(),
        file.display()
    );
    Ok(())
}

/// One line about a result.
pub fn summary(result: &BenchResult) -> String {
    let m = &result.measured;
    format!(
        "  {}: {} req/s, p50 {:.3} ms, p99 {:.3} ms, p99.9 {:.3} ms, hit ratio {:.1}%, {} errors, peak RSS {:.1} MiB, {:.1} CPU s, image {:.1} MiB",
        result.target.name,
        report::grouped(m.throughput_rps),
        m.latency.p50_ms,
        m.latency.p99_ms,
        m.latency.p999_ms,
        m.hit_ratio * 100.0,
        m.errors.connect + m.errors.timeout + m.errors.other,
        result.resources.peak_rss_bytes as f64 / (1 << 20) as f64,
        result.resources.cpu_seconds,
        result.target.image_size_bytes as f64 / (1 << 20) as f64
    )
}

fn render_report(args: &[String]) -> Result<(), Failure> {
    let flags = Flags::parse(args, &["results", "out", "title", "intro"], &[])?;
    flags.no_positional()?;
    let dir = flags.required("results")?;
    let out = flags.required("out")?;
    let results = report::load_dir(Path::new(dir)).map_err(failed)?;
    if results.is_empty() {
        return Err(failed(format!("no results under {dir}")));
    }
    let intro = match flags.one("intro")? {
        Some(path) => std::fs::read_to_string(path).map_err(|e| failed(format!("{path}: {e}")))?,
        None => String::new(),
    };
    let title = flags.one("title")?.unwrap_or("Benchmark report");
    std::fs::write(out, report::render(title, &intro, &results))
        .map_err(|e| failed(format!("{out}: {e}")))?;
    Ok(())
}

fn validate(args: &[String]) -> Result<(), Failure> {
    if args.is_empty() {
        return Err(usage("validate needs at least one file"));
    }
    let mut problems = Vec::new();
    for path in args {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                if let Err(e) = BenchResult::from_json(&text) {
                    problems.push(format!("{path}: {e}"));
                }
            }
            Err(e) => problems.push(format!("{path}: {e}")),
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(failed(problems.join("\n")))
    }
}

fn serve_origin(args: &[String]) -> Result<(), Failure> {
    let flags = Flags::parse(args, &["listen"], &[])?;
    flags.no_positional()?;
    let listen = flags.one("listen")?.unwrap_or("0.0.0.0:9000");
    let addr = listen
        .parse()
        .map_err(|_| usage(format!("--listen {listen:?} is not an address")))?;
    runtime()?.block_on(async {
        let origin = crate::origin::Origin::bind(addr)
            .await
            .map_err(|e| failed(format!("cannot listen on {listen}: {e}")))?;
        eprintln!("edgefront-bench origin on {}", origin.addr());
        wait_for_signal().await;
        Ok(())
    })
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn load(args: &[String]) -> Result<(), Failure> {
    let flags = Flags::parse(
        args,
        &["scenario", "target", "origin", "cgroup-root", "cgroup"],
        &[],
    )?;
    flags.no_positional()?;
    let scenario = read_scenario(flags.required("scenario")?)?;
    let options = LoadOptions {
        target: flags.required("target")?.to_owned(),
        origin: flags.required("origin")?.to_owned(),
        cgroup_root: PathBuf::from(flags.one("cgroup-root")?.unwrap_or("/sys/fs/cgroup")),
        cgroups: flags.all("cgroup").into_iter().map(str::to_owned).collect(),
        ready_within: targets::READY_WITHIN,
    };
    let outcome = runtime()?
        .block_on(targets::load_and_measure(&scenario, &options))
        .map_err(failed)?;
    let json = serde_json::to_string(&outcome).map_err(|e| failed(e.to_string()))?;
    println!("{json}");
    Ok(())
}

/// How long a whole run of `scenarios` against `targets` takes at least.
pub fn planned(scenarios: &[Scenario], targets: usize) -> Duration {
    scenarios
        .iter()
        .map(|s| s.schedule().duration())
        .sum::<Duration>()
        * targets as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("efb-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bad_usage_exits_2() {
        for args in [
            vec![],
            vec!["nope"],
            vec!["run"],
            vec!["run", "--scenario", "x"],
            vec!["run", "--scenario", "x", "--targets", ","],
            vec!["run", "--bogus"],
            vec!["run", "--scenario"],
            vec!["run", "extra"],
            vec!["report", "--results", "a"],
            vec!["report", "--results", "a", "--results", "b", "--out", "c"],
            vec!["validate"],
            vec!["origin", "--listen", "nowhere"],
            vec!["load", "--target", "x"],
        ] {
            let err = run(&strings(&args)).unwrap_err();
            assert!(matches!(err, Failure::Usage(_)), "{args:?}: {err:?}");
            assert_eq!(err.code(), ExitCode::from(2));
        }
        assert_eq!(failed("x").code(), ExitCode::from(1));
        run(&strings(&["--help"])).unwrap();
    }

    #[test]
    fn run_checks_its_inputs_before_touching_docker() {
        // An unreadable scenario fails.
        let err = run(&strings(&[
            "run",
            "--scenario",
            "/no/such.toml",
            "--targets",
            "varnish",
        ]))
        .unwrap_err();
        assert!(matches!(err, Failure::Failed(_)), "{err:?}");
        let scenario = format!(
            "{}/../../bench/scenarios/cache-hit.toml",
            env!("CARGO_MANIFEST_DIR")
        );
        let err = run(&strings(&[
            "run",
            "--scenario",
            &scenario,
            "--targets",
            "nonesuch",
        ]))
        .unwrap_err();
        assert!(
            matches!(&err, Failure::Usage(m) if m.contains("no target \"nonesuch\"") && m.contains("varnish")),
            "{err:?}"
        );
        let err = run(&strings(&[
            "run",
            "--scenario",
            &scenario,
            "--targets",
            "varnish",
            "--target-cpus",
            "0",
        ]))
        .unwrap_err();
        assert!(matches!(err, Failure::Usage(_)));
        assert!(find_root(Path::new("/")).is_none());
    }

    #[test]
    fn validates_and_renders_result_files() {
        let dir = scratch("report");
        let result = crate::report::tests::sample("varnish", "cache", "cache-hit");
        std::fs::create_dir_all(dir.join("results/cache-hit")).unwrap();
        let good = dir.join("results/cache-hit/varnish.json");
        std::fs::write(&good, result.to_json().unwrap()).unwrap();
        let good = good.display().to_string();
        run(&strings(&["validate", &good])).unwrap();
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{\"schema\": 1}").unwrap();
        let err = run(&strings(&[
            "validate",
            &good,
            &bad.display().to_string(),
            "/no/such.json",
        ]))
        .unwrap_err();
        assert!(
            matches!(&err, Failure::Failed(m) if m.contains("bad.json") && m.contains("such.json")),
            "{err:?}"
        );

        let intro = dir.join("intro.md");
        std::fs::write(&intro, "Hello.\n").unwrap();
        let out = dir.join("report.md");
        run(&strings(&[
            "report",
            "--results",
            &dir.join("results").display().to_string(),
            "--out",
            &out.display().to_string(),
            "--title",
            "T",
            "--intro",
            &intro.display().to_string(),
        ]))
        .unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.starts_with("# T\n\nHello.\n") && text.contains("| varnish |"));
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        let err = run(&strings(&[
            "report",
            "--results",
            &dir.join("empty").display().to_string(),
            "--out",
            "x",
        ]))
        .unwrap_err();
        assert!(matches!(&err, Failure::Failed(m) if m.contains("no results")));
        let err = run(&strings(&[
            "report",
            "--results",
            &dir.join("results").display().to_string(),
            "--out",
            "x",
            "--intro",
            "/no/intro",
        ]))
        .unwrap_err();
        assert!(matches!(err, Failure::Failed(_)));
        assert!(summary(&result).contains("varnish: 1,000 req/s"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_runs_against_a_local_origin_and_fails_cleanly() {
        let dir = scratch("load");
        let scenario = dir.join("s.toml");
        std::fs::write(
            &scenario,
            "name = \"s\"\nconnections = 2\n[load]\nrate = 50\nduration = \"200ms\"\n[keys]\ncount = 5\nzipf = 1.0\nhit_ratio = 0.0\n[[objects]]\nsize = 10\nweight = 1\n",
        )
        .unwrap();
        let scenario = scenario.display().to_string();
        let rt = runtime().unwrap();
        let origin = rt
            .block_on(crate::origin::Origin::bind("127.0.0.1:0".parse().unwrap()))
            .unwrap();
        let addr = origin.addr().to_string();
        // `load` builds its own runtime, so run it off this one's threads.
        let args = strings(&[
            "load",
            "--scenario",
            &scenario,
            "--target",
            &addr,
            "--origin",
            &addr,
        ]);
        std::thread::spawn(move || run(&args))
            .join()
            .unwrap()
            .unwrap();
        let args = strings(&[
            "load",
            "--scenario",
            &scenario,
            "--target",
            "no-such-host.invalid:1",
            "--origin",
            &addr,
        ]);
        let err = std::thread::spawn(move || run(&args))
            .join()
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, Failure::Failed(_)));
        let err = run(&strings(&[
            "load",
            "--scenario",
            "/no/such",
            "--target",
            "a:1",
            "--origin",
            "b:1",
        ]))
        .unwrap_err();
        assert!(matches!(err, Failure::Failed(_)));
        drop(origin);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_origin_refuses_a_taken_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let err = run(&strings(&["origin", "--listen", &addr])).unwrap_err();
        assert!(
            matches!(&err, Failure::Failed(m) if m.contains("cannot listen")),
            "{err:?}"
        );
    }

    #[test]
    fn a_run_plan_adds_up() {
        let s = Scenario::load(format!(
            "{}/../../bench/scenarios/cache-hit.toml",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        assert_eq!(planned(&[s], 2), Duration::from_secs(140));
    }
}
