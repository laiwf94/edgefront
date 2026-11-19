//! What a target costs: the CPUs it may use, its memory and CPU time, read
//! from its cgroup, and its image size.
//!
//! **CPU sets.** Every target of a run gets the same [`CpuSet`], and the
//! origin and the load generator get CPUs of their own, so a target never
//! competes with the traffic it serves. A target in a container is pinned
//! with `docker run --cpuset-cpus`, which sets the cgroup's
//! `cpuset.cpus`; a process run directly is pinned with `taskset -c`, which
//! sets its affinity. Both bound the same kernel scheduler mask, and a run
//! records the target's `cpuset.cpus.effective` so the pinning is checked,
//! not assumed.
//!
//! **Memory and CPU.** cgroup v2 keeps, for each container:
//!
//! - `memory.stat`: `anon` (heap, stacks, anonymous mappings) and
//!   `file_mapped` (file pages mapped into the processes). Their sum is the
//!   container's resident set, the RSS `ps` would add up across its
//!   processes. [`Sampler`] reads it every 100 ms and keeps the peak;
//! - `memory.peak`: the most memory the cgroup was ever charged, page cache
//!   and tmpfs included, so a target that caches on a tmpfs pays for it
//!   here even when its RSS is small;
//! - `cpu.stat`'s `usage_usec`: CPU time used by every process in the
//!   cgroup. The difference over the measured phase is the CPU seconds the
//!   measured requests cost.
//!
//! A target made of several containers (a proxy and its rate limit
//! service, say) is the sum of them all.
//!
//! The cgroup directory of a container depends on the cgroup driver:
//! `docker/<id>` under cgroupfs (Docker Desktop) and
//! `system.slice/docker-<id>.scope` under systemd (most Linux hosts).
//! [`Cgroup::find`] tries both under the hierarchy's root, which the load
//! generator sees when it runs with `--cgroupns=host`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A set of CPU numbers.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CpuSet(Vec<usize>);

impl CpuSet {
    /// The CPUs `first..first + count`.
    pub fn range(first: usize, count: usize) -> CpuSet {
        CpuSet((first..first + count).collect())
    }

    /// Parses the kernel's list format: `0-3,8,10-11`. The CPUs come out
    /// sorted and without repeats.
    pub fn parse(text: &str) -> Result<CpuSet, String> {
        let text = text.trim();
        let mut cpus = Vec::new();
        if text.is_empty() {
            return Ok(CpuSet(cpus));
        }
        let bad = || format!("{text:?} is not a CPU list such as 0-3,8");
        for part in text.split(',') {
            let (low, high) = match part.split_once('-') {
                Some((low, high)) => (low, high),
                None => (part, part),
            };
            let low: usize = low.trim().parse().map_err(|_| bad())?;
            let high: usize = high.trim().parse().map_err(|_| bad())?;
            if low > high || high >= 4096 {
                return Err(bad());
            }
            cpus.extend(low..=high);
        }
        cpus.sort_unstable();
        cpus.dedup();
        Ok(CpuSet(cpus))
    }

    /// How many CPUs.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The CPU numbers, sorted.
    pub fn cpus(&self) -> &[usize] {
        &self.0
    }

    /// The `docker run` argument that pins a container to the set.
    pub fn docker_arg(&self) -> String {
        format!("--cpuset-cpus={self}")
    }

    /// `program` and `args` run through `taskset -c`, pinned to the set.
    pub fn taskset(&self, program: &str, args: &[String]) -> std::process::Command {
        let mut command = std::process::Command::new("taskset");
        command
            .arg("-c")
            .arg(self.to_string())
            .arg(program)
            .args(args);
        command
    }

    /// Splits `available` CPUs between the target, the origin and the load
    /// generator: the target takes `target` of them (at most half), and
    /// the rest are halved between the other two, the load generator
    /// taking the odd one. Needs at least 3.
    pub fn split(available: usize, target: usize) -> Result<(CpuSet, CpuSet, CpuSet), String> {
        if available < 3 {
            return Err(format!(
                "{available} CPUs: the target, the origin and the load generator need one each"
            ));
        }
        let target = target.clamp(1, available / 2);
        let rest = available - target;
        let origin = rest / 2;
        Ok((
            CpuSet::range(0, target),
            CpuSet::range(target, origin),
            CpuSet::range(target + origin, rest - origin),
        ))
    }
}

impl fmt::Display for CpuSet {
    /// The kernel's list format, with runs collapsed: `0-3,8`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        let mut i = 0;
        while i < self.0.len() {
            let start = self.0[i];
            let mut end = start;
            while i + 1 < self.0.len() && self.0[i + 1] == end + 1 {
                i += 1;
                end = self.0[i];
            }
            if !first {
                f.write_str(",")?;
            }
            first = false;
            if start == end {
                write!(f, "{start}")?;
            } else {
                write!(f, "{start}-{end}")?;
            }
            i += 1;
        }
        Ok(())
    }
}

/// One container's cgroup v2 directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cgroup {
    dir: PathBuf,
}

/// One reading of a cgroup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CgroupReading {
    /// `cpu.stat` `usage_usec`.
    pub cpu_usec: u64,
    /// `memory.stat` `anon` plus `file_mapped`: the resident set.
    pub rss: u64,
    /// `memory.peak`, or 0 on a kernel without it (before 5.19).
    pub memory_peak: u64,
}

impl Cgroup {
    /// The cgroup at `dir`, as it is.
    pub fn at(dir: impl Into<PathBuf>) -> Cgroup {
        Cgroup { dir: dir.into() }
    }

    /// The cgroup of container `id` (the full 64-hex-digit id) under the
    /// hierarchy mounted at `root`.
    pub fn find(root: &Path, id: &str) -> Result<Cgroup, String> {
        if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{id:?} is not a full container id"));
        }
        let candidates = [
            root.join("docker").join(id),
            root.join("system.slice").join(format!("docker-{id}.scope")),
        ];
        candidates
            .iter()
            .find(|dir| dir.join("cpu.stat").is_file())
            .map(|dir| Cgroup::at(dir.clone()))
            .ok_or_else(|| {
                format!(
                    "no cgroup for container {} under {} (looked for {} and {})",
                    &id[..12],
                    root.display(),
                    candidates[0].display(),
                    candidates[1].display()
                )
            })
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Reads CPU time and memory now.
    pub fn read(&self) -> Result<CgroupReading, String> {
        let cpu = self.file("cpu.stat")?;
        let memory = self.file("memory.stat")?;
        let cpu_usec =
            field(&cpu, "usage_usec").ok_or_else(|| self.missing("cpu.stat", "usage_usec"))?;
        let anon = field(&memory, "anon").ok_or_else(|| self.missing("memory.stat", "anon"))?;
        let mapped = field(&memory, "file_mapped")
            .ok_or_else(|| self.missing("memory.stat", "file_mapped"))?;
        let memory_peak = match std::fs::read_to_string(self.dir.join("memory.peak")) {
            Ok(text) => text.trim().parse().unwrap_or(0),
            Err(_) => 0,
        };
        Ok(CgroupReading {
            cpu_usec,
            rss: anon + mapped,
            memory_peak,
        })
    }

    /// `cpuset.cpus.effective`: the CPUs the cgroup's processes may run on.
    pub fn effective_cpus(&self) -> Result<CpuSet, String> {
        CpuSet::parse(&self.file("cpuset.cpus.effective")?)
    }

    fn file(&self, name: &str) -> Result<String, String> {
        std::fs::read_to_string(self.dir.join(name))
            .map_err(|e| format!("{}: {e}", self.dir.join(name).display()))
    }

    fn missing(&self, file: &str, key: &str) -> String {
        format!("{} has no {key}", self.dir.join(file).display())
    }
}

/// The value of `key` in a flat-keyed cgroup file (`key value` lines).
fn field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (k, v) = line.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok()).flatten()
    })
}

/// What a target used over a run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Resources {
    /// The highest resident set seen, summed over the target's containers.
    pub peak_rss_bytes: u64,
    /// The sum of the containers' `memory.peak`: everything the target was
    /// ever charged, page cache and tmpfs included. 0 when the kernel does
    /// not keep it.
    pub peak_memory_bytes: u64,
    /// CPU time over the measured phase, in seconds.
    pub cpu_seconds: f64,
    /// `cpu_seconds` over the measured phase's wall time: the mean number
    /// of CPUs busy.
    pub cpu_cores: f64,
    /// The target's `cpuset.cpus.effective`, in list format.
    pub cpuset: String,
}

/// Samples a target's cgroups every 100 ms in a background thread, for the
/// peak RSS, and marks the measured phase's CPU time.
pub struct Sampler {
    cgroups: Vec<Cgroup>,
    peak: Arc<std::sync::Mutex<u64>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    mark: Option<(std::time::Instant, u64)>,
}

/// How often the sampler reads.
pub const SAMPLE_EVERY: Duration = Duration::from_millis(100);

impl Sampler {
    /// Starts sampling `cgroups`. Fails if any cannot be read.
    pub fn start(cgroups: Vec<Cgroup>) -> Result<Sampler, String> {
        let first = sum(&cgroups)?;
        let peak = Arc::new(std::sync::Mutex::new(first.rss));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let cgroups = cgroups.clone();
            let peak = Arc::clone(&peak);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(SAMPLE_EVERY);
                    if let Ok(reading) = sum(&cgroups) {
                        let mut peak = peak.lock().unwrap_or_else(|e| e.into_inner());
                        *peak = (*peak).max(reading.rss);
                    }
                }
            })
        };
        Ok(Sampler {
            cgroups,
            peak,
            stop,
            thread: Some(thread),
            mark: None,
        })
    }

    /// Marks the start of the measured phase.
    pub fn mark(&mut self) -> Result<(), String> {
        self.mark = Some((std::time::Instant::now(), sum(&self.cgroups)?.cpu_usec));
        Ok(())
    }

    /// Stops sampling and reports, with CPU time counted from the mark (or
    /// from the start, without one).
    pub fn finish(mut self) -> Result<Resources, String> {
        let end = sum(&self.cgroups)?;
        let now = std::time::Instant::now();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let peak = (*self.peak.lock().unwrap_or_else(|e| e.into_inner())).max(end.rss);
        let (since, usec) = self.mark.unwrap_or((now, end.cpu_usec));
        let cpu_seconds = end.cpu_usec.saturating_sub(usec) as f64 / 1e6;
        let wall = now.duration_since(since).as_secs_f64();
        let mut cpusets = Vec::new();
        for cgroup in &self.cgroups {
            cpusets.push(cgroup.effective_cpus()?.to_string());
        }
        cpusets.dedup();
        Ok(Resources {
            peak_rss_bytes: peak,
            peak_memory_bytes: end.memory_peak,
            cpu_seconds,
            cpu_cores: if wall > 0.0 { cpu_seconds / wall } else { 0.0 },
            cpuset: cpusets.join(" "),
        })
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn sum(cgroups: &[Cgroup]) -> Result<CgroupReading, String> {
    let mut total = CgroupReading::default();
    for cgroup in cgroups {
        let reading = cgroup.read()?;
        total.cpu_usec += reading.cpu_usec;
        total.rss += reading.rss;
        total.memory_peak += reading.memory_peak;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_lists_parse_and_print() {
        let set = CpuSet::parse("8,0-3, 2 ,10-11").unwrap();
        assert_eq!(set.cpus(), &[0, 1, 2, 3, 8, 10, 11]);
        assert_eq!(set.to_string(), "0-3,8,10-11");
        assert_eq!(set.len(), 7);
        assert_eq!(set.docker_arg(), "--cpuset-cpus=0-3,8,10-11");
        assert_eq!(CpuSet::parse("5").unwrap().to_string(), "5");
        assert!(CpuSet::parse("").unwrap().is_empty());
        assert_eq!(CpuSet::parse("\n").unwrap().to_string(), "");
        for bad in ["a", "3-1", "1-", "-1", "0-5000", "1,,2"] {
            assert!(CpuSet::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn taskset_wraps_the_command() {
        let command = CpuSet::range(4, 2).taskset("edgefront", &["--config".into(), "x".into()]);
        assert_eq!(command.get_program(), "taskset");
        let args: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args, ["-c", "4-5", "edgefront", "--config", "x"]);
    }

    #[test]
    fn splits_cpus_between_the_three() {
        let (t, o, l) = CpuSet::split(16, 4).unwrap();
        assert_eq!(
            (t.to_string(), o.to_string(), l.to_string()),
            ("0-3".into(), "4-9".into(), "10-15".into())
        );
        let (t, o, l) = CpuSet::split(4, 4).unwrap();
        assert_eq!(
            (t.to_string(), o.to_string(), l.to_string()),
            ("0-1".into(), "2".into(), "3".into())
        );
        let (t, o, l) = CpuSet::split(3, 0).unwrap();
        assert_eq!((t.len(), o.len(), l.len()), (1, 1, 1));
        assert!(CpuSet::split(2, 1).is_err());
    }

    fn fake(dir: &Path, cpu: u64, anon: u64, mapped: u64, peak: Option<u64>) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("cpu.stat"),
            format!("usage_usec {cpu}\nuser_usec 1\nsystem_usec 2\n"),
        )
        .unwrap();
        std::fs::write(
            dir.join("memory.stat"),
            format!("anon {anon}\nfile 999\nkernel 5\nfile_mapped {mapped}\nshmem 7\n"),
        )
        .unwrap();
        match peak {
            Some(p) => std::fs::write(dir.join("memory.peak"), format!("{p}\n")).unwrap(),
            None => {
                let _ = std::fs::remove_file(dir.join("memory.peak"));
            }
        }
        std::fs::write(dir.join("cpuset.cpus.effective"), "0-3\n").unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("edgefront-bench-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn finds_cgroups_under_both_drivers() {
        let root = scratch("find");
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        fake(&root.join("docker").join(&a), 1, 2, 3, Some(4));
        fake(
            &root.join("system.slice").join(format!("docker-{b}.scope")),
            1,
            2,
            3,
            Some(4),
        );
        assert_eq!(
            Cgroup::find(&root, &a).unwrap().dir(),
            root.join("docker").join(&a)
        );
        assert!(
            Cgroup::find(&root, &b)
                .unwrap()
                .dir()
                .ends_with(format!("docker-{b}.scope"))
        );
        let err = Cgroup::find(&root, &"c".repeat(64)).unwrap_err();
        assert!(
            err.contains("no cgroup for container cccccccccccc"),
            "{err}"
        );
        assert!(Cgroup::find(&root, "short").is_err());
        assert!(Cgroup::find(&root, &"z".repeat(64)).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn reads_and_fails_clearly() {
        let root = scratch("read");
        fake(&root, 1_500_000, 100, 20, Some(4096));
        let cgroup = Cgroup::at(&root);
        assert_eq!(
            cgroup.read().unwrap(),
            CgroupReading {
                cpu_usec: 1_500_000,
                rss: 120,
                memory_peak: 4096
            }
        );
        assert_eq!(cgroup.effective_cpus().unwrap().to_string(), "0-3");
        fake(&root, 1, 1, 1, None);
        assert_eq!(cgroup.read().unwrap().memory_peak, 0);
        std::fs::write(root.join("memory.stat"), "anon 1\n").unwrap();
        assert!(cgroup.read().unwrap_err().contains("file_mapped"));
        std::fs::write(root.join("memory.stat"), "file_mapped 1\n").unwrap();
        assert!(cgroup.read().unwrap_err().contains("anon"));
        std::fs::write(root.join("cpu.stat"), "user_usec 1\n").unwrap();
        assert!(cgroup.read().unwrap_err().contains("usage_usec"));
        std::fs::remove_dir_all(&root).unwrap();
        assert!(cgroup.read().unwrap_err().contains("cpu.stat"));
        assert!(Sampler::start(vec![cgroup]).is_err());
    }

    #[test]
    fn the_sampler_keeps_the_peak_and_counts_from_the_mark() {
        let root = scratch("sampler");
        let a = root.join("a");
        let b = root.join("b");
        fake(&a, 1_000_000, 1000, 0, Some(5000));
        fake(&b, 0, 10, 0, Some(50));
        let mut sampler = Sampler::start(vec![Cgroup::at(&a), Cgroup::at(&b)]).unwrap();
        // A spike between samples that the next sample sees.
        fake(&a, 1_000_000, 9000, 1000, Some(20_000));
        std::thread::sleep(SAMPLE_EVERY * 3);
        fake(&a, 2_000_000, 1000, 0, Some(20_000));
        sampler.mark().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        fake(&a, 3_500_000, 500, 0, Some(20_000));
        fake(&b, 500_000, 10, 0, Some(50));
        let resources = sampler.finish().unwrap();
        assert_eq!(resources.peak_rss_bytes, 10_010);
        assert_eq!(resources.peak_memory_bytes, 20_050);
        assert!((resources.cpu_seconds - 2.0).abs() < 1e-9, "{resources:?}");
        assert!(resources.cpu_cores > 0.0);
        assert_eq!(resources.cpuset, "0-3");

        // Without a mark, nothing is counted.
        let sampler = Sampler::start(vec![Cgroup::at(&a)]).unwrap();
        let resources = sampler.finish().unwrap();
        assert_eq!(resources.cpu_seconds, 0.0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn flat_keyed_fields() {
        assert_eq!(field("a 1\nb 2\n", "b"), Some(2));
        assert_eq!(field("a 1\nbb 2\n", "b"), None);
        assert_eq!(field("a x\n", "a"), None);
    }
}
