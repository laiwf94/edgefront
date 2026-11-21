//! The competitor runner: one command runs a scenario against edgefront
//! and the products it is measured against, each in a pinned container.
//!
//! A target is a directory under `bench/targets/<name>/` holding
//! `target.toml` and the product's config files:
//!
//! ```toml
//! product = "Varnish Cache"
//! version = "9.0.4"
//! kind = "cache"                  # or "proxy": it does not cache
//! track = "cache"                 # cache, proxy or limiter; default cache
//! image = "varnish:9.0.4@sha256:…" # pinned by digest
//! port = 80
//! command = ["varnishd", "-F", "-a", ":80", "-s", "malloc,{{cache_mib}}M", …]
//! tmpfs = ["/var/lib/varnish:exec"]
//!
//! [files]                         # container path = file in this directory
//! "/etc/varnish/default.vcl" = "default.vcl"
//!
//! [[sidecars]]                    # more containers the target needs
//! name = "redis"                  # reachable by this name
//! image = "redis:…@sha256:…"
//! ```
//!
//! An image built from this repository names its Dockerfile stage instead
//! of a digest (`build = { dockerfile = "bench/Dockerfile", stage =
//! "edgefront" }`); it is pinned by the commit it is built from.
//!
//! Every string in `command`, `env`, `tmpfs` and the config files may use
//! `{{origin}}` (`origin:9000`), `{{origin_host}}`, `{{origin_port}}`,
//! `{{port}}`, `{{cpus}}` (the target's CPU count), `{{cache_bytes}}` and
//! `{{cache_mib}}` (the cache size every target gets: the scenario's
//! working set plus a quarter, at least 64 MiB), and `{{max_object_bytes}}`
//! and `{{max_object_mib}}` (the scenario's largest object, at least
//! 1 MiB). So every target caches the same bytes, of the same objects,
//! for the TTL the origin sends ([`crate::origin`]).
//!
//! **A run.** [`Runner::run`] creates a private network, starts the
//! origin, then the sidecars and the target, pinned to their CPU sets
//! ([`crate::measure::CpuSet`]), and runs the load generator in a container
//! of its own on the same network (`edgefront-bench load`, see
//! [`load_and_measure`]), so the load never crosses the host's port
//! forwarding. The load generator waits for the target, reads the
//! target's cgroups through `--cgroupns=host` while it loads, and prints
//! what it saw as JSON; the runner adds the images' sizes and the machine,
//! checks the result against its schema and writes it to
//! `<out>/<scenario>/<target>.json`. Every container and the network are
//! removed afterwards, whether the run worked or not.
//!
//! The container runtime is reached through the [`Runtime`] trait;
//! [`Docker`] is the `docker` command line.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::client;
use crate::load::{self, LoadConfig};
use crate::measure::{Cgroup, CpuSet, Resources, Sampler};
use crate::origin;
use crate::report::{
    BenchResult, Image, Machine, Measured, PhaseSummary, SCHEMA, ScenarioInfo, TargetInfo,
};
use crate::scenario::Scenario;
use crate::tls::TlsClient;

/// The port the origin listens on inside the network.
pub const ORIGIN_PORT: u16 = 9000;

/// The path a load run polls until the target answers it. One object, of
/// one byte, so readiness costs every target the same single fill.
pub const READY_PATH: &str = "/obj/1/ready";

/// How long a target may take to answer [`READY_PATH`].
pub const READY_WITHIN: Duration = Duration::from_secs(90);

/// The image holding `edgefront-bench` itself, for the origin and the load
/// generator.
pub const HARNESS_IMAGE: &str = "edgefront-bench/harness:local";

/// The container that is the runtime-wide benchmark lock.
pub const LOCK: &str = "edgefront-bench-lock";

/// This host's name, as `hostname` prints it.
fn host() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Whether a lock owner, `pid@host`, is a process on this host that no
/// longer runs. An owner on another host, or one that cannot be read, is
/// never taken for dead.
fn is_dead(owner: &str) -> bool {
    let Some((pid, owner_host)) = owner.split_once('@') else {
        return false;
    };
    if owner_host != host() || pid.parse::<u32>().is_err() {
        return false;
    }
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| !s.success())
}

/// The smallest cache any target gets.
const MIN_CACHE: u64 = 64 << 20;

/// A Dockerfile stage built from this repository.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    /// The Dockerfile, relative to the repository root.
    pub dockerfile: String,
    /// The stage to build.
    pub stage: String,
}

/// One container of a target.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Container {
    /// The name other containers reach it by. The target itself is
    /// always `target`.
    #[serde(default)]
    pub name: String,
    /// The image, pinned by digest, or the local tag a `build` makes.
    pub image: String,
    /// How to build `image` from this repository, when it is local.
    pub build: Option<Build>,
    /// Replaces the image's entrypoint.
    pub entrypoint: Option<String>,
    /// The command; empty keeps the image's.
    #[serde(default)]
    pub command: Vec<String>,
    /// Environment variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// tmpfs mounts, as `path[:options]`.
    #[serde(default)]
    pub tmpfs: Vec<String>,
    /// Config files: container path to a file in the target's directory.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// The user to run as.
    pub user: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetFile {
    product: String,
    version: String,
    kind: String,
    #[serde(default = "default_track")]
    track: String,
    port: u16,
    image: String,
    build: Option<Build>,
    entrypoint: Option<String>,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    tmpfs: Vec<String>,
    #[serde(default)]
    files: BTreeMap<String, String>,
    user: Option<String>,
    #[serde(default)]
    sidecars: Vec<Container>,
    /// The path of the target's binary in its image, whose size the result
    /// records as `extra.binary_bytes`.
    binary: Option<String>,
}

fn default_track() -> String {
    "cache".into()
}

/// A target, read from its directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// The directory's name.
    pub name: String,
    /// The directory.
    pub dir: PathBuf,
    /// The product.
    pub product: String,
    /// Its version.
    pub version: String,
    /// `cache` or `proxy`.
    pub kind: String,
    /// `cache`, `proxy` or `limiter`.
    pub track: String,
    /// The port it serves on.
    pub port: u16,
    /// Its own container.
    pub container: Container,
    /// The containers it needs, started first, in order.
    pub sidecars: Vec<Container>,
    /// The path of its binary in the image, for `extra.binary_bytes`.
    pub binary: Option<String>,
}

impl Target {
    /// Reads `<dir>/target.toml`, checking every field and that every
    /// config file it names exists.
    pub fn load(dir: &Path) -> Result<Target, String> {
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("{}: not a target directory", dir.display()))?
            .to_owned();
        let path = dir.join("target.toml");
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let file: TargetFile =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let bad = |why: String| format!("{}: {why}", path.display());
        if !matches!(file.kind.as_str(), "cache" | "proxy") {
            return Err(bad(format!("kind {:?} must be cache or proxy", file.kind)));
        }
        if !matches!(file.track.as_str(), "cache" | "proxy" | "limiter") {
            return Err(bad(format!(
                "track {:?} must be cache, proxy or limiter",
                file.track
            )));
        }
        if file.port == 0 {
            return Err(bad("port must be above 0".into()));
        }
        for text in [&file.product, &file.version] {
            if text.trim().is_empty() {
                return Err(bad("product and version must not be empty".into()));
            }
        }
        let container = Container {
            name: "target".into(),
            image: file.image,
            build: file.build,
            entrypoint: file.entrypoint,
            command: file.command,
            env: file.env,
            tmpfs: file.tmpfs,
            files: file.files,
            user: file.user,
        };
        let mut names = vec!["target".to_owned(), "origin".to_owned(), "load".to_owned()];
        for sidecar in &file.sidecars {
            if sidecar.name.is_empty()
                || !sidecar
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                return Err(bad(format!(
                    "sidecar name {:?} must be lower-case letters, digits and -",
                    sidecar.name
                )));
            }
            if names.contains(&sidecar.name) {
                return Err(bad(format!("sidecar name {:?} is taken", sidecar.name)));
            }
            names.push(sidecar.name.clone());
        }
        for c in std::iter::once(&container).chain(&file.sidecars) {
            check_image(&c.image, c.build.is_some()).map_err(bad)?;
            for (mount, source) in &c.files {
                if !mount.starts_with('/') {
                    return Err(bad(format!(
                        "file mount {mount:?} must be an absolute path"
                    )));
                }
                if source.contains("..") || source.starts_with('/') {
                    return Err(bad(format!(
                        "file {source:?} must be inside the target's directory"
                    )));
                }
                if !dir.join(source).is_file() {
                    return Err(bad(format!("{source} is missing")));
                }
            }
        }
        if let Some(b) = &file.binary
            && !b.starts_with('/')
        {
            return Err(bad(format!("binary {b:?} must be an absolute path")));
        }
        Ok(Target {
            name,
            dir: dir.to_owned(),
            product: file.product,
            version: file.version,
            kind: file.kind,
            track: file.track,
            port: file.port,
            container,
            sidecars: file.sidecars,
            binary: file.binary,
        })
    }

    /// Reads every target under `dir`, sorted by name.
    pub fn load_all(dir: &Path) -> Result<Vec<Target>, String> {
        let mut targets = Vec::new();
        let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
            if path.join("target.toml").is_file() {
                targets.push(Target::load(&path)?);
            }
        }
        targets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(targets)
    }

    /// The containers in start order: sidecars, then the target.
    pub fn containers(&self) -> impl Iterator<Item = &Container> {
        self.sidecars.iter().chain(std::iter::once(&self.container))
    }
}

/// An image is pinned by digest unless the repository builds it.
fn check_image(image: &str, built: bool) -> Result<(), String> {
    if image.trim().is_empty() {
        return Err("image must not be empty".into());
    }
    if built {
        if image.contains('@') {
            return Err(format!(
                "{image:?} is built here, so it takes a tag, not a digest"
            ));
        }
        return Ok(());
    }
    match image.split_once("@sha256:") {
        Some((_, digest))
            if digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            Ok(())
        }
        _ => Err(format!(
            "{image:?} must be pinned as name:tag@sha256:<64 hex digits>"
        )),
    }
}

/// The values the templates are filled from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vars(BTreeMap<String, String>);

impl Vars {
    /// The values for `scenario` on a target with `port` and `cpus` CPUs.
    pub fn new(scenario: &Scenario, port: u16, cpus: usize) -> Vars {
        let cache = cache_bytes(scenario);
        let max_object = scenario
            .objects
            .iter()
            .map(|o| o.size)
            .max()
            .unwrap_or(0)
            .max(1 << 20);
        let mut vars = BTreeMap::new();
        let mut set = |k: &str, v: String| {
            vars.insert(k.to_owned(), v);
        };
        set("origin", format!("origin:{ORIGIN_PORT}"));
        set("origin_host", "origin".into());
        set("origin_port", ORIGIN_PORT.to_string());
        set("port", port.to_string());
        set("cpus", cpus.to_string());
        set("cache_bytes", cache.to_string());
        set("cache_mib", (cache >> 20).to_string());
        set("max_object_bytes", max_object.to_string());
        set("max_object_mib", max_object.div_ceil(1 << 20).to_string());
        Vars(vars)
    }

    /// Fills every `{{name}}` in `text`. An unknown name is an error.
    pub fn render(&self, text: &str) -> Result<String, String> {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let end = after
                .find("}}")
                .ok_or_else(|| "an unclosed {{".to_owned())?;
            let name = after[..end].trim();
            let value = self
                .0
                .get(name)
                .ok_or_else(|| format!("unknown template value {{{{{name}}}}}"))?;
            out.push_str(value);
            rest = &after[end + 2..];
        }
        out.push_str(rest);
        Ok(out)
    }
}

/// The cache every target gets for `scenario`: its working set plus a
/// quarter, rounded up to a MiB, and at least 64 MiB.
pub fn cache_bytes(scenario: &Scenario) -> u64 {
    let wanted = scenario.working_set() + scenario.working_set() / 4;
    wanted.div_ceil(1 << 20).max(MIN_CACHE >> 20) << 20
}

/// A container runtime: runs one `docker` command line and returns its
/// standard output.
pub trait Runtime {
    /// Runs the runtime with `args`. A non-zero exit is an error carrying
    /// its standard error.
    fn exec(&mut self, args: &[String]) -> Result<String, String>;

    /// The last `lines` lines a container wrote, standard output and
    /// standard error together; empty when there are none to read.
    fn logs(&mut self, container: &str, lines: usize) -> String {
        self.exec(&args(&["logs", "--tail", &lines.to_string(), container]))
            .unwrap_or_default()
    }
}

/// The `docker` command line.
#[derive(Clone, Debug, Default)]
pub struct Docker;

impl Runtime for Docker {
    fn logs(&mut self, container: &str, lines: usize) -> String {
        match std::process::Command::new("docker")
            .args(["logs", "--tail", &lines.to_string(), container])
            .stdin(std::process::Stdio::null())
            .output()
        {
            Ok(output) => format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(_) => String::new(),
        }
    }

    fn exec(&mut self, args: &[String]) -> Result<String, String> {
        let output = std::process::Command::new("docker")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("cannot run docker: {e}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(format!(
                "docker {} failed: {}",
                args.first().map(String::as_str).unwrap_or(""),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

/// How the runner splits the machine and where it writes.
#[derive(Clone, Debug)]
pub struct RunnerConfig {
    /// The repository root, where builds run.
    pub root: PathBuf,
    /// CPUs for each target.
    pub target_cpus: usize,
    /// The image with `edgefront-bench` for the origin and load generator.
    pub harness_image: String,
    /// The run's name.
    pub run: String,
    /// Where results go.
    pub out: PathBuf,
    /// The machine as a reader knows it ("16-core arm64, Docker 28").
    pub machine: String,
    /// The command line that started the run, recorded in each result.
    pub command: String,
    /// The tag for images built from this repository in place of `local`
    /// ([`local_tag`]).
    pub local_tag: String,
    /// How long to wait for another benchmark holding the runtime.
    pub lock_wait: Duration,
    /// How often to look while waiting.
    pub lock_poll: Duration,
}

/// The tag for this checkout's own images: `EDGEFRONT_BENCH_TAG` when set
/// (CI sets `local`, for the images it builds itself), otherwise `local-`
/// and a hash of the repository root, so each checkout on a machine builds
/// and runs its own.
pub fn local_tag(root: &Path) -> String {
    if let Ok(tag) = std::env::var("EDGEFRONT_BENCH_TAG")
        && !tag.is_empty()
    {
        return tag;
    }
    // FNV-1a: stable across Rust releases, unlike the std hasher.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.display().to_string().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("local-{:012x}", hash & 0xffff_ffff_ffff)
}

/// Runs scenarios against targets through a [`Runtime`].
pub struct Runner<R: Runtime> {
    runtime: R,
    config: RunnerConfig,
    machine: Machine,
    cpus: (CpuSet, CpuSet, CpuSet),
    sequence: u32,
}

/// What `edgefront-bench load` prints: the load generator's side of a
/// result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadOutcome {
    /// The measured phases.
    pub measured: Measured,
    /// Every phase.
    pub phases: Vec<PhaseSummary>,
    /// What the target used.
    pub resources: Resources,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Info {
    #[serde(rename = "NCPU")]
    ncpu: u64,
    mem_total: u64,
    kernel_version: String,
    architecture: String,
    server_version: String,
}

impl<R: Runtime> Runner<R> {
    /// Reads the machine from the runtime and splits its CPUs.
    pub fn new(mut runtime: R, config: RunnerConfig) -> Result<Runner<R>, String> {
        let text = runtime.exec(&args(&["info", "--format", "{{json .}}"]))?;
        let info: Info =
            serde_json::from_str(text.trim()).map_err(|e| format!("docker info: {e}"))?;
        let cpus = CpuSet::split(info.ncpu as usize, config.target_cpus)?;
        let major = info.server_version.split('.').next().unwrap_or("");
        let description = if config.machine.is_empty() {
            let arch = match info.architecture.as_str() {
                "aarch64" => "arm64",
                other => other,
            };
            format!("{}-core {arch}, Docker {major}", info.ncpu)
        } else {
            config.machine.clone()
        };
        let machine = Machine {
            description,
            cpus: info.ncpu,
            memory_bytes: info.mem_total,
            kernel: info.kernel_version,
            arch: info.architecture,
            runtime: format!("docker {}", info.server_version),
            target_cpus: cpus.0.to_string(),
            origin_cpus: cpus.1.to_string(),
            loadgen_cpus: cpus.2.to_string(),
        };
        Ok(Runner {
            runtime,
            config,
            machine,
            cpus,
            sequence: 0,
        })
    }

    /// The machine as results record it.
    pub fn machine(&self) -> &Machine {
        &self.machine
    }

    /// The image a container runs: its own reference, except that an image
    /// this repository builds, tagged `:local`, takes the runner's
    /// [`RunnerConfig::local_tag`] instead, so two checkouts sharing one
    /// container runtime never run each other's builds.
    pub fn image(&self, container: &Container) -> String {
        match container.image.strip_suffix(":local") {
            Some(name) if container.build.is_some() => format!("{name}:{}", self.config.local_tag),
            _ => container.image.clone(),
        }
    }

    fn harness_image(&self) -> String {
        match self.config.harness_image.strip_suffix(":local") {
            Some(name) => format!("{name}:{}", self.config.local_tag),
            None => self.config.harness_image.clone(),
        }
    }

    /// Makes sure every image is present: pulls pinned ones, builds local
    /// ones. `rebuild` builds local images even when they exist.
    pub fn prepare(&mut self, targets: &[Target], rebuild: bool) -> Result<(), String> {
        let harness = Container {
            name: "harness".into(),
            image: self.config.harness_image.clone(),
            build: Some(Build {
                dockerfile: "bench/Dockerfile".into(),
                stage: "harness".into(),
            }),
            entrypoint: None,
            command: vec![],
            env: BTreeMap::new(),
            tmpfs: vec![],
            files: BTreeMap::new(),
            user: None,
        };
        let mut seen = Vec::new();
        for c in std::iter::once(&harness).chain(targets.iter().flat_map(Target::containers)) {
            let image = self.image(c);
            if seen.contains(&image) {
                continue;
            }
            seen.push(image.clone());
            let present = self
                .runtime
                .exec(&args(&["image", "inspect", "--format", "{{.Id}}", &image]))
                .is_ok();
            match &c.build {
                Some(build) if rebuild || !present => {
                    let dockerfile = self.config.root.join(&build.dockerfile);
                    self.runtime.exec(&[
                        "build".into(),
                        "--file".into(),
                        dockerfile.display().to_string(),
                        "--target".into(),
                        build.stage.clone(),
                        "--tag".into(),
                        image.clone(),
                        self.config.root.display().to_string(),
                    ])?;
                }
                None if !present => {
                    self.runtime.exec(&args(&["pull", "--quiet", &image]))?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The size of the file at `path` in `image`, copied out of a
    /// container made from the image and never started.
    fn binary_size(&mut self, prefix: &str, image: &str, path: &str) -> Result<u64, String> {
        let name = format!("{prefix}-binary");
        let dir = std::env::temp_dir().join(&name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let copy = dir.join("binary");
        let result = self
            .runtime
            .exec(&args(&["create", "--name", &name, image]))
            .and_then(|_| {
                self.runtime.exec(&args(&[
                    "cp",
                    &format!("{name}:{path}"),
                    &copy.display().to_string(),
                ]))
            })
            .and_then(|_| {
                std::fs::metadata(&copy)
                    .map(|m| m.len())
                    .map_err(|e| format!("{image}:{path}: {e}"))
            });
        let _ = self.runtime.exec(&args(&["rm", "--force", &name]));
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    /// Runs `scenario` (read from `scenario_path`) against `target` and
    /// writes its result. Every container is removed afterwards.
    pub fn run(
        &mut self,
        scenario: &Scenario,
        scenario_path: &Path,
        target: &Target,
    ) -> Result<BenchResult, String> {
        let stem = target.name.clone();
        self.run_as(scenario, scenario_path, target, &stem)
    }

    /// As [`Runner::run`], writing `<out>/<scenario>/<stem>.json`: one of
    /// several repeats, say.
    pub fn run_as(
        &mut self,
        scenario: &Scenario,
        scenario_path: &Path,
        target: &Target,
        stem: &str,
    ) -> Result<BenchResult, String> {
        self.lock()?;
        let result = self.run_unlocked(scenario, scenario_path, target, stem);
        let _ = self.runtime.exec(&args(&["rm", "--force", LOCK]));
        result
    }

    /// Takes the runtime-wide benchmark lock: the container [`LOCK`],
    /// which only one process can create. Two runs on one container runtime
    /// would share its CPUs and measure each other, so a run waits for the
    /// one before it, up to [`RunnerConfig::lock_wait`]. A lock whose owner
    /// is a process on this host that has died is taken over.
    fn lock(&mut self) -> Result<(), String> {
        let owner = format!("{}@{}", std::process::id(), host());
        let harness = self.harness_image();
        let deadline = std::time::Instant::now() + self.config.lock_wait;
        let mut said = false;
        loop {
            match self.runtime.exec(&[
                "create".into(),
                "--name".into(),
                LOCK.into(),
                "--label".into(),
                format!("edgefront-bench.owner={owner}"),
                harness.clone(),
            ]) {
                Ok(_) => return Ok(()),
                Err(e) if !e.contains("already in use") => {
                    return Err(format!("taking the benchmark lock: {e}"));
                }
                Err(_) => {}
            }
            let holder = self
                .runtime
                .exec(&args(&[
                    "inspect",
                    "--format",
                    "{{index .Config.Labels \"edgefront-bench.owner\"}}",
                    LOCK,
                ]))
                .unwrap_or_default()
                .trim()
                .to_owned();
            if is_dead(&holder) {
                eprintln!("edgefront-bench: taking over the lock of {holder}, which has exited");
                let _ = self.runtime.exec(&args(&["rm", "--force", LOCK]));
                continue;
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "another benchmark ({holder}) has held the runtime for over {:?}; remove the container {LOCK} if it is stale",
                    self.config.lock_wait
                ));
            }
            if !said {
                eprintln!("edgefront-bench: waiting for another benchmark ({holder}) to finish");
                said = true;
            }
            std::thread::sleep(self.config.lock_poll);
        }
    }

    fn run_unlocked(
        &mut self,
        scenario: &Scenario,
        scenario_path: &Path,
        target: &Target,
        stem: &str,
    ) -> Result<BenchResult, String> {
        self.sequence += 1;
        let prefix = format!("efb-{}-{}", std::process::id(), self.sequence);
        let mut created = Vec::new();
        let network = format!("{prefix}-net");
        self.runtime.exec(&args(&["network", "create", &network]))?;
        let started = SystemTime::now();
        let outcome = self.run_in(
            &prefix,
            &network,
            scenario,
            scenario_path,
            target,
            &mut created,
        );
        // Logs first, while the target still exists, for the error.
        let outcome = match outcome {
            Ok(o) => Ok(o),
            Err(e) => {
                let logs = self.runtime.logs(&format!("{prefix}-target"), 30);
                Err(if logs.trim().is_empty() {
                    format!("{}: {e}", target.name)
                } else {
                    format!(
                        "{}: {e}\nthe target's last log lines:\n{}",
                        target.name,
                        logs.trim_end()
                    )
                })
            }
        };
        for name in created.iter().rev() {
            let _ = self
                .runtime
                .exec(&args(&["rm", "--force", "--volumes", name]));
        }
        let _ = self.runtime.exec(&args(&["network", "rm", &network]));
        let (outcome, images) = outcome?;
        let image_size_bytes = images.iter().map(|i| i.size_bytes).sum();
        let mut extra = BTreeMap::new();
        if let Some(path) = &target.binary {
            let bytes = self.binary_size(&prefix, &target.container.image, path)?;
            extra.insert("binary_bytes".to_owned(), bytes as f64);
        }
        let result = BenchResult {
            schema: SCHEMA.into(),
            run: self.config.run.clone(),
            track: target.track.clone(),
            started_at: crate::report::rfc3339(started),
            command: self.config.command.clone(),
            scenario: ScenarioInfo::of(scenario),
            target: TargetInfo {
                name: target.name.clone(),
                product: target.product.clone(),
                version: target.version.clone(),
                kind: target.kind.clone(),
                images,
                image_size_bytes,
            },
            machine: self.machine.clone(),
            measured: outcome.measured,
            phases: outcome.phases,
            resources: outcome.resources,
            extra,
        };
        if result.resources.cpuset != self.machine.target_cpus {
            return Err(format!(
                "{}: the target ran on CPUs {:?}, not the {} it was pinned to",
                target.name, result.resources.cpuset, self.machine.target_cpus
            ));
        }
        let json = result.to_json()?;
        let dir = self.config.out.join(&scenario.name);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let file = dir.join(format!("{stem}.json"));
        std::fs::write(&file, json).map_err(|e| format!("{}: {e}", file.display()))?;
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_in(
        &mut self,
        prefix: &str,
        network: &str,
        scenario: &Scenario,
        scenario_path: &Path,
        target: &Target,
        created: &mut Vec<String>,
    ) -> Result<(LoadOutcome, Vec<Image>), String> {
        let (target_cpus, origin_cpus, load_cpus) = self.cpus.clone();
        let vars = Vars::new(scenario, target.port, target_cpus.len());
        let harness = self.harness_image();

        let origin = format!("{prefix}-origin");
        let mut command = vec!["create".into(), "--name".into(), origin.clone()];
        command.extend(network_args(network, "origin"));
        command.push(origin_cpus.docker_arg());
        command.push(harness.clone());
        command.extend(origin_args(scenario));
        self.runtime.exec(&command)?;
        created.push(origin.clone());
        self.runtime.exec(&args(&["start", &origin]))?;

        let mut ids = Vec::new();
        for container in target.containers() {
            let name = format!("{prefix}-{}", container.name);
            let id = self.start(
                &name,
                network,
                container,
                &target_cpus,
                &vars,
                &target.dir,
                created,
            )?;
            ids.push(id);
        }

        let load = format!("{prefix}-load");
        let mut command = vec!["create".into(), "--name".into(), load.clone()];
        command.extend(network_args(network, "load"));
        command.push(load_cpus.docker_arg());
        command.push("--cgroupns=host".into());
        command.push(harness);
        command.extend(args(&[
            "load",
            "--scenario",
            "/scenario.toml",
            "--name",
            &scenario.name,
            "--target",
            &format!("target:{}", target.port),
            "--origin",
            &format!("origin:{ORIGIN_PORT}"),
            "--cgroup-root",
            "/sys/fs/cgroup",
        ]));
        for id in &ids {
            command.push("--cgroup".into());
            command.push(id.clone());
        }
        self.runtime.exec(&command)?;
        created.push(load.clone());
        self.runtime.exec(&args(&[
            "cp",
            &scenario_path.display().to_string(),
            &format!("{load}:/scenario.toml"),
        ]))?;
        let out = self.runtime.exec(&args(&["start", "--attach", &load]))?;
        let line = out.lines().rev().find(|l| l.starts_with('{')).unwrap_or("");
        let outcome: LoadOutcome = serde_json::from_str(line)
            .map_err(|e| format!("the load generator's output: {e}: {}", out.trim()))?;

        let mut images = Vec::new();
        let refs: Vec<String> = target.containers().map(|c| self.image(c)).collect();
        for image in refs {
            let text = self.runtime.exec(&args(&[
                "image",
                "inspect",
                "--format",
                "{{.Id}} {{.Size}}",
                &image,
            ]))?;
            let (id, size) = text
                .trim()
                .split_once(' ')
                .ok_or_else(|| format!("docker image inspect {image}: {text:?}"))?;
            images.push(Image {
                reference: image.clone(),
                id: id.to_owned(),
                size_bytes: size
                    .parse()
                    .map_err(|_| format!("docker image inspect {image}: {text:?}"))?,
            });
        }
        // The target first, then its sidecars.
        images.rotate_right(1);
        Ok((outcome, images))
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        name: &str,
        network: &str,
        container: &Container,
        cpus: &CpuSet,
        vars: &Vars,
        dir: &Path,
        created: &mut Vec<String>,
    ) -> Result<String, String> {
        let mut command = vec!["create".into(), "--name".into(), name.to_owned()];
        command.extend(network_args(network, &container.name));
        command.push(cpus.docker_arg());
        if let Some(entrypoint) = &container.entrypoint {
            command.push(format!("--entrypoint={}", vars.render(entrypoint)?));
        }
        if let Some(user) = &container.user {
            command.push(format!("--user={user}"));
        }
        for (key, value) in &container.env {
            command.push("--env".into());
            command.push(format!("{key}={}", vars.render(value)?));
        }
        for mount in &container.tmpfs {
            command.push("--tmpfs".into());
            command.push(vars.render(mount)?);
        }
        command.push(self.image(container));
        for arg in &container.command {
            command.push(vars.render(arg)?);
        }
        let id = self.runtime.exec(&command)?.trim().to_owned();
        created.push(name.to_owned());
        if !container.files.is_empty() {
            let staging = std::env::temp_dir().join(format!("{name}-files"));
            let _ = std::fs::remove_dir_all(&staging);
            let result = self.copy_files(name, container, vars, dir, &staging);
            let _ = std::fs::remove_dir_all(&staging);
            result?;
        }
        self.runtime.exec(&args(&["start", name]))?;
        Ok(id)
    }

    fn copy_files(
        &mut self,
        name: &str,
        container: &Container,
        vars: &Vars,
        dir: &Path,
        staging: &Path,
    ) -> Result<(), String> {
        std::fs::create_dir_all(staging).map_err(|e| format!("{}: {e}", staging.display()))?;
        for (index, (mount, source)) in container.files.iter().enumerate() {
            let text =
                std::fs::read_to_string(dir.join(source)).map_err(|e| format!("{source}: {e}"))?;
            let rendered = vars.render(&text).map_err(|e| format!("{source}: {e}"))?;
            let local = staging.join(index.to_string());
            std::fs::write(&local, rendered).map_err(|e| format!("{}: {e}", local.display()))?;
            self.runtime.exec(&args(&[
                "cp",
                &local.display().to_string(),
                &format!("{name}:{mount}"),
            ]))?;
        }
        Ok(())
    }
}

/// The origin's command line for `scenario`: its listen address, and the
/// scenario's `origin_delay` when it has one.
pub fn origin_args(scenario: &Scenario) -> Vec<String> {
    let mut out = args(&["origin", "--listen", &format!("0.0.0.0:{ORIGIN_PORT}")]);
    if !scenario.origin_delay.is_zero() {
        out.push("--delay".into());
        out.push(format!("{}ms", scenario.origin_delay.as_secs_f64() * 1e3));
    }
    out
}

fn network_args(network: &str, alias: &str) -> Vec<String> {
    vec![
        format!("--network={network}"),
        format!("--network-alias={alias}"),
        // Room for the connections of the heaviest scenario.
        "--ulimit=nofile=65536:65536".into(),
    ]
}

/// Where `edgefront-bench load` points and what it reads.
#[derive(Clone, Debug)]
pub struct LoadOptions {
    /// The target, `host:port`.
    pub target: String,
    /// The origin, `host:port`.
    pub origin: String,
    /// The cgroup hierarchy's root.
    pub cgroup_root: PathBuf,
    /// The full ids of the target's containers; none measures nothing.
    pub cgroups: Vec<String>,
    /// How long to wait for the target to answer [`READY_PATH`].
    pub ready_within: Duration,
}

async fn resolve(address: &str) -> Result<SocketAddr, String> {
    tokio::net::lookup_host(address)
        .await
        .map_err(|e| format!("{address}: {e}"))?
        .next()
        .ok_or_else(|| format!("{address}: no address"))
}

/// The load generator's side of a run: waits for the target, loads it
/// with the scenario while sampling its cgroups, and counts the origin's
/// requests over the measured phase.
pub async fn load_and_measure(
    scenario: &Scenario,
    options: &LoadOptions,
) -> Result<LoadOutcome, String> {
    let target = resolve(&options.target).await?;
    let origin_addr = resolve(&options.origin).await?;
    let tls = scenario
        .tls
        .as_deref()
        .map(|name| TlsClient::new(name, scenario.protocol))
        .transpose()?;
    // The readiness probe speaks HTTP/1.1, whatever the load speaks.
    let probe = scenario
        .tls
        .as_deref()
        .map(|name| TlsClient::new(name, crate::load::Protocol::Http1))
        .transpose()?;
    client::wait_ready_over(target, READY_PATH, options.ready_within, probe.as_ref())
        .await
        .map_err(|e| format!("the target at {}: {e}", options.target))?;
    let mut cgroups = Vec::new();
    for id in &options.cgroups {
        cgroups.push(Cgroup::find(&options.cgroup_root, id)?);
    }
    let sampler = if cgroups.is_empty() {
        None
    } else {
        Some(Arc::new(Mutex::new(Some(Sampler::start(cgroups)?))))
    };
    let before = origin::fetch_stats(origin_addr).await?;
    let warmup: Duration = scenario
        .schedule()
        .phases()
        .iter()
        .filter(|p| p.warmup)
        .map(|p| p.duration)
        .sum();
    let marker = {
        let sampler = sampler.clone();
        tokio::spawn(async move {
            tokio::time::sleep(warmup).await;
            let stats = origin::fetch_stats(origin_addr).await;
            if let Some(sampler) = sampler
                && let Some(s) = sampler.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
            {
                s.mark()?;
            }
            stats
        })
    };
    let mut config = LoadConfig::new(target, scenario.protocol, scenario.schedule());
    config.authority = options.target.clone();
    config.connections = scenario.connections;
    config.timeout = scenario.timeout;
    config.tls = tls;
    config.grpc = scenario.kind == crate::scenario::RequestKind::Grpc;
    let report = load::run(config, scenario.requests())
        .await
        .map_err(|e| e.to_string())?;
    let at_mark = marker.await.map_err(|e| e.to_string())??;
    let after = origin::fetch_stats(origin_addr).await?;
    let resources = match sampler {
        Some(sampler) => match sampler.lock().unwrap_or_else(|e| e.into_inner()).take() {
            Some(s) => s.finish()?,
            None => Resources::default(),
        },
        None => Resources::default(),
    };
    let origin_requests = if warmup.is_zero() {
        after.since(&before)
    } else {
        after.since(&at_mark)
    };
    Ok(LoadOutcome {
        measured: Measured::of(&report.measured(), origin_requests.requests),
        phases: report.phases.iter().map(PhaseSummary::of).collect(),
        resources,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    pub(crate) fn scenario() -> Scenario {
        Scenario::from_toml(
            r#"
            name = "tiny"
            description = "A tiny scenario."
            connections = 2
            timeout = "2s"
            [warmup]
            rate = 200
            duration = "300ms"
            [load]
            rate = 400
            duration = "500ms"
            [keys]
            count = 20
            zipf = 1.0
            hit_ratio = 0.5
            [[objects]]
            size = "2KiB"
            weight = 1
            "#,
        )
        .unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("efb-targets-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const DIGEST: &str = "@sha256:1111111111111111111111111111111111111111111111111111111111111111";

    fn write_target(root: &Path, name: &str, toml: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("target.toml"), toml).unwrap();
        for (file, text) in files {
            std::fs::write(dir.join(file), text).unwrap();
        }
        dir
    }

    #[test]
    fn reads_targets_and_refuses_bad_ones() {
        let root = scratch("load");
        let good = format!(
            r#"
            product = "Thing"
            version = "1.2"
            kind = "cache"
            image = "thing:1.2{DIGEST}"
            port = 80
            command = ["thing", "--size", "{{{{cache_mib}}}}M"]
            [files]
            "/etc/thing.conf" = "thing.conf"
            [[sidecars]]
            name = "redis"
            image = "redis:8{DIGEST}"
            "#
        );
        let dir = write_target(
            &root,
            "thing",
            &good,
            &[("thing.conf", "origin {{origin}}\n")],
        );
        let target = Target::load(&dir).unwrap();
        assert_eq!(target.name, "thing");
        assert_eq!(target.track, "cache");
        assert_eq!(
            target
                .containers()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["redis", "target"]
        );
        write_target(
            &root,
            "built",
            "product = \"edgefront\"\nversion = \"0.1.0\"\nkind = \"cache\"\ntrack = \"cache\"\nport = 8080\nimage = \"edgefront-bench/edgefront:local\"\nbuild = { dockerfile = \"bench/Dockerfile\", stage = \"edgefront\" }\n",
            &[],
        );
        std::fs::create_dir_all(root.join("not-a-target")).unwrap();
        let all = Target::load_all(&root).unwrap();
        assert_eq!(
            all.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["built", "thing"]
        );

        let cases: &[(&str, &str)] = &[
            ("kind = \"cache\"", "kind = \"store\""),
            ("version = \"1.2\"", "version = \"1.2\"\ntrack = \"other\""),
            ("port = 80", "port = 0"),
            ("version = \"1.2\"", "version = \" \""),
            (DIGEST, ":latest"),
            ("\"/etc/thing.conf\"", "\"etc/thing.conf\""),
            ("= \"thing.conf\"", "= \"../thing.conf\""),
            ("= \"thing.conf\"", "= \"missing.conf\""),
            ("name = \"redis\"", "name = \"Redis\""),
            ("name = \"redis\"", "name = \"origin\""),
            ("port = 80", "port = 80\nsurprise = 1"),
        ];
        for (from, to) in cases {
            let text = good.replacen(from, to, 1);
            let dir = write_target(&root, "bad", &text, &[("thing.conf", "x")]);
            assert!(Target::load(&dir).is_err(), "{to}");
        }
        let dir = write_target(
            &root,
            "bad",
            "product = \"x\"\nversion = \"1\"\nkind = \"cache\"\nport = 1\nimage = \"x@sha256:00\"\nbuild = { dockerfile = \"d\", stage = \"s\" }\n",
            &[],
        );
        assert!(Target::load(&dir).unwrap_err().contains("takes a tag"));
        assert!(Target::load(&root.join("absent")).is_err());
        assert!(Target::load_all(&root.join("absent")).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_origin_holds_requests_for_the_scenarios_delay() {
        let mut s = scenario();
        assert_eq!(origin_args(&s), ["origin", "--listen", "0.0.0.0:9000"]);
        s.origin_delay = Duration::from_millis(200);
        let with = origin_args(&s);
        assert_eq!(
            with,
            ["origin", "--listen", "0.0.0.0:9000", "--delay", "200ms"]
        );
        assert_eq!(
            crate::scenario::parse_duration(&with[4]),
            Some(Duration::from_millis(200))
        );
        s.origin_delay = Duration::from_micros(1500);
        assert_eq!(
            crate::scenario::parse_duration(&origin_args(&s)[4]),
            Some(Duration::from_micros(1500))
        );
    }

    #[test]
    fn templates_fill_every_value_and_refuse_unknown_ones() {
        let vars = Vars::new(&scenario(), 8080, 4);
        assert_eq!(
            vars.render("{{origin}} {{ origin_host }}:{{origin_port}} p{{port}} c{{cpus}} {{cache_mib}}M {{cache_bytes}} {{max_object_mib}} {{max_object_bytes}}").unwrap(),
            "origin:9000 origin:9000 p8080 c4 64M 67108864 1 1048576"
        );
        assert!(vars.render("{{nope}}").unwrap_err().contains("nope"));
        assert!(vars.render("{{origin").is_err());
        assert_eq!(vars.render("no braces").unwrap(), "no braces");
    }

    #[test]
    fn the_cache_covers_the_working_set() {
        let s = Scenario::load(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/scenarios/web-mix.toml"),
        )
        .unwrap();
        let bytes = cache_bytes(&s);
        assert!(bytes >= s.working_set() * 5 / 4 && bytes.is_multiple_of(1 << 20));
        assert_eq!(cache_bytes(&scenario()), 64 << 20);
    }

    /// A runtime that answers from a script and records every command.
    struct Fake {
        log: Vec<Vec<String>>,
        answers: VecDeque<(&'static str, Result<String, String>)>,
    }

    impl Runtime for Fake {
        fn exec(&mut self, args: &[String]) -> Result<String, String> {
            self.log.push(args.to_vec());
            let line = args.join(" ");
            if let Some(i) = self
                .answers
                .iter()
                .position(|(prefix, _)| line.starts_with(prefix))
            {
                return self.answers.remove(i).unwrap().1;
            }
            Ok(String::new())
        }
    }

    const INFO: &str = r#"{"NCPU":16,"MemTotal":8217190400,"KernelVersion":"6.10.14-linuxkit","Architecture":"aarch64","ServerVersion":"28.0.4","Other":1}"#;

    fn outcome(cpuset: &str) -> String {
        let measured = Measured::of(
            &crate::load::RunReport::measured(&crate::load::RunReport {
                protocol: crate::load::Protocol::Http1,
                connections: 1,
                phases: vec![],
                elapsed: Duration::ZERO,
            }),
            0,
        );
        let mut m = measured;
        m.scheduled = 200;
        m.completed = 200;
        m.throughput_rps = 400.0;
        serde_json::to_string(&LoadOutcome {
            measured: m,
            phases: vec![],
            resources: Resources {
                peak_rss_bytes: 1,
                peak_memory_bytes: 2,
                cpu_seconds: 0.5,
                cpu_cores: 1.0,
                cpuset: cpuset.into(),
            },
        })
        .unwrap()
    }

    fn config(root: &Path) -> RunnerConfig {
        RunnerConfig {
            root: root.to_owned(),
            target_cpus: 4,
            harness_image: HARNESS_IMAGE.into(),
            run: "test-run".into(),
            out: root.join("out"),
            machine: String::new(),
            command: "edgefront-bench run …".into(),
            local_tag: "local-test".into(),
            lock_wait: Duration::from_millis(300),
            lock_poll: Duration::from_millis(50),
        }
    }

    #[test]
    fn a_run_starts_everything_pinned_and_removes_it_after() {
        let root = scratch("run");
        let toml = format!(
            "product = \"Thing\"\nversion = \"1.2\"\nkind = \"cache\"\nimage = \"thing:1.2{DIGEST}\"\nport = 80\nentrypoint = \"/bin/thing\"\nuser = \"nobody\"\ncommand = [\"--cache\", \"{{{{cache_mib}}}}M\"]\ntmpfs = [\"/cache\"]\n[env]\nTHREADS = \"{{{{cpus}}}}\"\n[files]\n\"/etc/thing.conf\" = \"thing.conf\"\n[[sidecars]]\nname = \"helper\"\nimage = \"helper:1{DIGEST}\"\n"
        );
        let dir = write_target(
            &root.join("targets"),
            "thing",
            &toml,
            &[("thing.conf", "upstream {{origin}}\n")],
        );
        let target = Target::load(&dir).unwrap();
        let scenario_path = root.join("tiny.toml");
        std::fs::write(&scenario_path, "x").unwrap();
        let fake = Fake {
            log: vec![],
            answers: VecDeque::from([
                ("info", Ok(INFO.into())),
                ("create --name efb-", Ok("ignored".into())),
                (
                    "start --attach",
                    Ok(format!("a log line\n{}\n", outcome("0-3"))),
                ),
                (
                    "image inspect --format {{.Id}} {{.Size}} helper",
                    Ok("sha256:h 100\n".into()),
                ),
                (
                    "image inspect --format {{.Id}} {{.Size}} thing",
                    Ok("sha256:t 250\n".into()),
                ),
            ]),
        };
        let mut runner = Runner::new(fake, config(&root)).unwrap();
        assert_eq!(runner.machine().description, "16-core arm64, Docker 28");
        assert_eq!(runner.machine().target_cpus, "0-3");
        let result = runner.run(&scenario(), &scenario_path, &target).unwrap();
        assert_eq!(result.target.image_size_bytes, 350);
        assert_eq!(
            result.target.images[0].reference,
            format!("thing:1.2{DIGEST}")
        );
        assert_eq!(result.measured.completed, 200);
        assert_eq!(result.scenario.name, "tiny");
        let written = std::fs::read_to_string(root.join("out/tiny/thing.json")).unwrap();
        assert_eq!(BenchResult::from_json(&written).unwrap(), result);

        let log: Vec<String> = runner.runtime.log.iter().map(|a| a.join(" ")).collect();
        let find = |needle: &str| {
            log.iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle} in {log:#?}"))
        };
        let net = find("network create");
        let origin = find("origin --listen 0.0.0.0:9000");
        let helper = find("--network-alias=helper");
        let thing = find("--network-alias=target");
        let load = find(" load --scenario /scenario.toml --name tiny --target target:80");
        // No origin_delay, no --delay.
        assert!(log[origin].ends_with("origin --listen 0.0.0.0:9000"));
        assert!(net < origin && origin < helper && helper < thing && thing < load);
        assert!(log[origin].contains("--cpuset-cpus=4-9"));
        assert!(log[thing].contains("--cpuset-cpus=0-3"));
        assert!(log[helper].contains("--cpuset-cpus=0-3"));
        assert!(log[load].contains("--cpuset-cpus=10-15") && log[load].contains("--cgroupns=host"));
        assert!(
            log[thing]
                .contains("--entrypoint=/bin/thing --user=nobody --env THREADS=4 --tmpfs /cache")
        );
        assert!(log[thing].ends_with(&format!("thing:1.2{DIGEST} --cache 64M")));
        assert!(log.iter().any(|l| l.starts_with("cp ")
            && !l.ends_with("efb-")
            && l.contains("-target:/etc/thing.conf")));
        assert!(log.iter().any(|l| l.contains(":/scenario.toml")));
        // Removed in reverse order, then the network.
        let removes: Vec<&String> = log
            .iter()
            .filter(|l| l.starts_with("rm --force --volumes"))
            .collect();
        assert_eq!(removes.len(), 4);
        assert!(removes[0].ends_with("-load") && removes[3].ends_with("-origin"));
        assert!(log[log.len() - 2].starts_with("network rm"));
        // The lock is taken first and given back last.
        assert!(log.iter().any(|l| {
            l.starts_with("create --name edgefront-bench-lock --label edgefront-bench.owner=")
        }));
        assert_eq!(log.last().unwrap(), "rm --force edgefront-bench-lock");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_run_reports_the_target_logs_and_still_cleans_up() {
        let root = scratch("fail");
        let dir = write_target(
            &root,
            "thing",
            &format!(
                "product = \"Thing\"\nversion = \"1\"\nkind = \"proxy\"\nimage = \"thing:1{DIGEST}\"\nport = 80\n"
            ),
            &[],
        );
        let target = Target::load(&dir).unwrap();
        let fake = Fake {
            log: vec![],
            answers: VecDeque::from([
                ("info", Ok(INFO.into())),
                ("start --attach", Err("docker start failed: exit 1".into())),
                ("logs --tail 30", Ok("boom: bad config\n".into())),
            ]),
        };
        let mut runner = Runner::new(fake, config(&root)).unwrap();
        let err = runner
            .run(&scenario(), &root.join("s.toml"), &target)
            .unwrap_err();
        assert!(
            err.contains("thing: docker start failed") && err.contains("boom: bad config"),
            "{err}"
        );
        let log: Vec<String> = runner.runtime.log.iter().map(|a| a.join(" ")).collect();
        assert_eq!(
            log.iter()
                .filter(|l| l.starts_with("rm --force --volumes"))
                .count(),
            3
        );
        assert!(log[log.len() - 2].starts_with("network rm"));
        // The lock is taken first and given back last.
        assert!(log.iter().any(|l| {
            l.starts_with("create --name edgefront-bench-lock --label edgefront-bench.owner=")
        }));
        assert_eq!(log.last().unwrap(), "rm --force edgefront-bench-lock");

        // A target not on its CPUs is refused.
        let fake = Fake {
            log: vec![],
            answers: VecDeque::from([
                ("info", Ok(INFO.into())),
                ("start --attach", Ok(outcome("0-15"))),
                ("image inspect", Ok("sha256:t 250".into())),
            ]),
        };
        let mut runner = Runner::new(fake, config(&root)).unwrap();
        let err = runner
            .run(&scenario(), &root.join("s.toml"), &target)
            .unwrap_err();
        assert!(err.contains("not the 0-3 it was pinned to"), "{err}");

        // Garbage from the load generator, or from image inspect.
        for answers in [
            vec![
                ("info", Ok(INFO.to_owned())),
                ("start --attach", Ok("no json".to_owned())),
            ],
            vec![
                ("info", Ok(INFO.to_owned())),
                ("start --attach", Ok(outcome("0-3"))),
                ("image inspect", Ok("weird".to_owned())),
            ],
            vec![
                ("info", Ok(INFO.to_owned())),
                ("start --attach", Ok(outcome("0-3"))),
                ("image inspect", Ok("id size".to_owned())),
            ],
        ] {
            let fake = Fake {
                log: vec![],
                answers: answers.into(),
            };
            let mut runner = Runner::new(fake, config(&root)).unwrap();
            assert!(
                runner
                    .run(&scenario(), &root.join("s.toml"), &target)
                    .is_err()
            );
        }
        // A runtime without enough CPUs, or with no info.
        let small = INFO.replace("\"NCPU\":16", "\"NCPU\":2");
        assert!(
            Runner::new(
                Fake {
                    log: vec![],
                    answers: VecDeque::from([("info", Ok(small))])
                },
                config(&root)
            )
            .is_err()
        );
        assert!(
            Runner::new(
                Fake {
                    log: vec![],
                    answers: VecDeque::from([("info", Ok("{}".into()))])
                },
                config(&root)
            )
            .is_err()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn prepare_pulls_pinned_images_and_builds_local_ones() {
        let root = scratch("prepare");
        let pinned = write_target(
            &root,
            "pinned",
            &format!(
                "product = \"P\"\nversion = \"1\"\nkind = \"cache\"\nimage = \"p:1{DIGEST}\"\nport = 80\n"
            ),
            &[],
        );
        let built = write_target(
            &root,
            "built",
            "product = \"B\"\nversion = \"1\"\nkind = \"cache\"\nimage = \"b:local\"\nbuild = { dockerfile = \"bench/Dockerfile\", stage = \"edgefront\" }\nport = 80\n",
            &[],
        );
        let targets = vec![
            Target::load(&pinned).unwrap(),
            Target::load(&built).unwrap(),
            Target::load(&pinned).unwrap(),
        ];
        let fake = Fake {
            log: vec![],
            answers: VecDeque::from([
                ("info", Ok(INFO.into())),
                (
                    "image inspect --format {{.Id}} edgefront-bench/harness:local",
                    Ok("sha256:x".into()),
                ),
                (
                    "image inspect --format {{.Id}} p:1",
                    Err("no such image".into()),
                ),
                (
                    "image inspect --format {{.Id}} b:local",
                    Err("no such image".into()),
                ),
            ]),
        };
        let mut runner = Runner::new(fake, config(&root)).unwrap();
        runner.prepare(&targets, false).unwrap();
        let log: Vec<String> = runner.runtime.log.iter().map(|a| a.join(" ")).collect();
        assert!(
            log.iter()
                .any(|l| l == &format!("pull --quiet p:1{DIGEST}"))
        );
        assert!(log.iter().any(|l| l.starts_with("build --file")
            && l.contains("--target edgefront --tag b:local-test")));
        assert!(!log.iter().any(|l| l.contains("--target harness")));
        assert_eq!(
            log.iter()
                .filter(|l| l.starts_with("image inspect"))
                .count(),
            3
        );

        // Rebuild builds the local ones even when present.
        let fake = Fake {
            log: vec![],
            answers: VecDeque::from([("info", Ok(INFO.into()))]),
        };
        let mut runner = Runner::new(fake, config(&root)).unwrap();
        runner.prepare(&targets, true).unwrap();
        let log: Vec<String> = runner.runtime.log.iter().map(|a| a.join(" ")).collect();
        assert!(log.iter().any(|l| l.contains("--target harness")));
        assert!(!log.iter().any(|l| l.starts_with("pull")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn each_checkout_tags_its_own_images() {
        let a = local_tag(Path::new("/work/a"));
        let b = local_tag(Path::new("/work/b"));
        if std::env::var("EDGEFRONT_BENCH_TAG").is_err() {
            assert!(a.starts_with("local-") && a.len() == 18, "{a}");
            assert_ne!(a, b);
            assert_eq!(a, local_tag(Path::new("/work/a")));
        }
        let mut built = scenario_container("edgefront-bench/edgefront:local");
        built.build = Some(Build {
            dockerfile: "bench/Dockerfile".into(),
            stage: "edgefront".into(),
        });
        let pinned = scenario_container(&format!("varnish:9{DIGEST}"));
        let root = scratch("tags");
        let fake = Fake {
            log: vec![],
            answers: VecDeque::from([("info", Ok(INFO.into()))]),
        };
        let runner = Runner::new(fake, config(&root)).unwrap();
        assert_eq!(runner.image(&built), "edgefront-bench/edgefront:local-test");
        assert_eq!(runner.image(&pinned), format!("varnish:9{DIGEST}"));
        assert_eq!(runner.harness_image(), "edgefront-bench/harness:local-test");
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn scenario_container(image: &str) -> Container {
        Container {
            name: "target".into(),
            image: image.into(),
            build: None,
            entrypoint: None,
            command: vec![],
            env: BTreeMap::new(),
            tmpfs: vec![],
            files: BTreeMap::new(),
            user: None,
        }
    }

    #[test]
    fn a_held_lock_is_waited_for_and_a_dead_one_taken_over() {
        let root = scratch("lock");
        let dir = write_target(
            &root,
            "thing",
            &format!(
                "product = \"Thing\"\nversion = \"1\"\nkind = \"proxy\"\nimage = \"thing:1{DIGEST}\"\nport = 80\n"
            ),
            &[],
        );
        let target = Target::load(&dir).unwrap();
        let held = || {
            Err::<String, String>(
                "Conflict. The container name \"/edgefront-bench-lock\" is already in use".into(),
            )
        };
        // Held by a live process on another host: wait, then give up.
        let fake = Fake {
            log: vec![],
            answers: VecDeque::from([
                ("info", Ok(INFO.into())),
                ("create --name edgefront-bench-lock", held()),
                ("create --name edgefront-bench-lock", held()),
                ("create --name edgefront-bench-lock", held()),
                ("create --name edgefront-bench-lock", held()),
                ("create --name edgefront-bench-lock", held()),
                ("create --name edgefront-bench-lock", held()),
                ("create --name edgefront-bench-lock", held()),
                ("create --name edgefront-bench-lock", held()),
                ("create --name edgefront-bench-lock", held()),
                ("create --name edgefront-bench-lock", held()),
                ("inspect", Ok("1@elsewhere\n".into())),
                ("inspect", Ok("1@elsewhere\n".into())),
                ("inspect", Ok("1@elsewhere\n".into())),
                ("inspect", Ok("1@elsewhere\n".into())),
                ("inspect", Ok("1@elsewhere\n".into())),
                ("inspect", Ok("1@elsewhere\n".into())),
                ("inspect", Ok("1@elsewhere\n".into())),
                ("inspect", Ok("1@elsewhere\n".into())),
                ("inspect", Ok("1@elsewhere\n".into())),
                ("inspect", Ok("1@elsewhere\n".into())),
            ]),
        };
        let mut runner = Runner::new(fake, config(&root)).unwrap();
        let err = runner
            .run(&scenario(), &root.join("s.toml"), &target)
            .unwrap_err();
        assert!(err.contains("another benchmark (1@elsewhere)"), "{err}");
        assert!(!runner.runtime.log.iter().any(|a| a[0] == "network"));

        // Held by a process of this host that has exited: taken over.
        let dead = format!("4000000@{}", host());
        assert!(is_dead(&dead));
        assert!(!is_dead(&format!("{}@{}", std::process::id(), host())));
        assert!(!is_dead("nonsense") && !is_dead("x@y"));
        let fake = Fake {
            log: vec![],
            answers: VecDeque::from([
                ("info", Ok(INFO.into())),
                ("create --name edgefront-bench-lock", held()),
                ("inspect", Ok(dead.clone())),
                ("start --attach", Ok(outcome("0-3"))),
                ("image inspect", Ok("sha256:t 250".into())),
            ]),
        };
        let mut runner = Runner::new(fake, config(&root)).unwrap();
        runner
            .run(&scenario(), &root.join("s.toml"), &target)
            .unwrap();
        let log: Vec<String> = runner.runtime.log.iter().map(|a| a.join(" ")).collect();
        let creates = log
            .iter()
            .filter(|l| l.starts_with("create --name edgefront-bench-lock"))
            .count();
        assert_eq!(creates, 2, "{log:#?}");

        // Any other failure to create the lock stops the run.
        let fake = Fake {
            log: vec![],
            answers: VecDeque::from([
                ("info", Ok(INFO.into())),
                (
                    "create --name edgefront-bench-lock",
                    Err("docker create failed: no such image".into()),
                ),
            ]),
        };
        let mut runner = Runner::new(fake, config(&root)).unwrap();
        let err = runner
            .run(&scenario(), &root.join("s.toml"), &target)
            .unwrap_err();
        assert!(err.contains("taking the benchmark lock"), "{err}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn docker_errors_name_the_command() {
        // `docker` with a bad flag fails, or is absent: either way an error.
        let err = Docker
            .exec(&args(&["--no-such-flag-for-the-harness"]))
            .unwrap_err();
        assert!(err.contains("docker"), "{err}");
        // No such container: whatever docker says, or nothing.
        let _ = Docker.logs("efb-no-such-container", 5);
    }

    #[tokio::test]
    async fn loads_and_measures_against_a_local_origin() {
        let origin = origin::Origin::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let options = LoadOptions {
            target: origin.addr().to_string(),
            origin: origin.addr().to_string(),
            cgroup_root: PathBuf::from("/nonexistent"),
            cgroups: vec![],
            ready_within: Duration::from_secs(5),
        };
        let outcome = load_and_measure(&scenario(), &options).await.unwrap();
        assert_eq!(outcome.measured.scheduled, 200);
        assert_eq!(outcome.measured.completed, 200);
        // No cache in front: every measured request reached the origin.
        assert!(
            outcome.measured.origin_requests >= 190,
            "{:?}",
            outcome.measured
        );
        assert!(outcome.measured.hit_ratio < 0.06);
        assert_eq!(outcome.phases.len(), 2);
        assert_eq!(outcome.resources, Resources::default());

        let bad = LoadOptions {
            cgroups: vec!["a".repeat(64)],
            ..options.clone()
        };
        assert!(
            load_and_measure(&scenario(), &bad)
                .await
                .unwrap_err()
                .contains("no cgroup")
        );
        let nothing = LoadOptions {
            target: "127.0.0.1:1".into(),
            ready_within: Duration::from_millis(200),
            ..options.clone()
        };
        assert!(
            load_and_measure(&scenario(), &nothing)
                .await
                .unwrap_err()
                .contains("not ready")
        );
        let unresolvable = LoadOptions {
            origin: "no-such-host.invalid:1".into(),
            ..options
        };
        assert!(load_and_measure(&scenario(), &unresolvable).await.is_err());
    }
}
