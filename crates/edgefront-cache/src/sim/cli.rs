//! The `cache_sim` command line: replays a trace through policies at cache
//! sizes and prints one result per (policy, size).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;

use super::trace::{self, CsvColumns, Format, Trace};
use super::{Footprint, POLICIES, Replay, Request, Simulation, Stats};

pub const USAGE: &str = "\
usage: cache_sim [options] TRACE

Replays TRACE (a file, or - for standard input) through each policy at each
cache size, and prints hit ratio, byte hit ratio and writes per pair.

  -s, --sizes LIST      cache sizes, comma-separated: bytes with an optional
                        unit (KiB, MiB, GiB, TiB, KB, MB, GB, TB), or a share
                        of the trace's footprint (0.1%, 10%), which reads the
                        trace once more first; required
  -p, --policies LIST   policies to run (default: all of s3fifo, lru, sieve,
                        wtinylfu)
      --format F        oracleGeneral or csv (default: csv when the name
                        contains .csv, else oracleGeneral)
      --columns SPEC    CSV columns, 1-based (default time=1,id=2,size=3;
                        - leaves one out)
      --delimiter C     CSV delimiter: one character, tab or space (default ,)
      --header          the CSV's first line is a header
      --ignore-size     count every object as 1 byte
      --limit N         replay at most the first N requests
      --threads N       worker threads (default: the CPUs available)
      --output F        table, csv or json (default table)
  -h, --help            print this

Files ending .zst or .gz are decompressed as they are read.";

/// Runs `cache_sim` with `args` (without the program name), writing results
/// to `out` and errors to `err`.
pub fn main(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    match run(args, out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Usage(why)) => {
            let _ = writeln!(err, "cache_sim: {why}\n\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Error::Run(why)) => {
            let _ = writeln!(err, "cache_sim: {why}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// The command line is wrong.
    Usage(String),
    /// The run failed: the trace could not be read, or output written.
    Run(String),
}

fn usage(why: impl Into<String>) -> Error {
    Error::Usage(why.into())
}

/// A cache size as given.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Size {
    Bytes(u64),
    /// A share of the footprint, from 0 to 1.
    Share(f64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Table,
    Csv,
    Json,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub trace: PathBuf,
    pub format: Format,
    pub sizes: Vec<(String, Size)>,
    pub policies: Vec<&'static str>,
    pub ignore_size: bool,
    pub limit: Option<u64>,
    pub threads: usize,
    pub output: Output,
}

/// Parses the command line. `Ok(None)` asks for the usage text.
pub fn parse(args: &[String]) -> Result<Option<Options>, Error> {
    let mut trace = None;
    let mut sizes = None;
    let mut policies: Vec<&'static str> = POLICIES.to_vec();
    let mut format = None;
    let mut columns = None;
    let mut delimiter = b',';
    let mut header = false;
    let mut ignore_size = false;
    let mut limit = None;
    let mut threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut output = Output::Table;

    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .map(String::as_str)
                .ok_or_else(|| usage(format!("{name} needs a value")))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-s" | "--sizes" => sizes = Some(parse_sizes(value(arg)?)?),
            "-p" | "--policies" => policies = parse_policies(value(arg)?)?,
            "--format" => {
                format = Some(match value(arg)?.to_ascii_lowercase().as_str() {
                    "oraclegeneral" | "oracle" => "oracle",
                    "csv" => "csv",
                    other => return Err(usage(format!("unknown format `{other}`"))),
                })
            }
            "--columns" => columns = Some(value(arg)?.to_string()),
            "--delimiter" => {
                delimiter = match value(arg)? {
                    "tab" | "\\t" | "\t" => b'\t',
                    "space" | " " => b' ',
                    d if d.len() == 1 && d.is_ascii() => d.as_bytes()[0],
                    d => return Err(usage(format!("delimiter `{d}` is not one character"))),
                }
            }
            "--header" => header = true,
            "--ignore-size" => ignore_size = true,
            "--limit" => limit = Some(count(arg, value(arg)?)?),
            "--threads" => threads = count(arg, value(arg)?)? as usize,
            "--output" => {
                output = match value(arg)? {
                    "table" => Output::Table,
                    "csv" => Output::Csv,
                    "json" => Output::Json,
                    other => return Err(usage(format!("unknown output `{other}`"))),
                }
            }
            a if a.starts_with('-') && a != "-" => {
                return Err(usage(format!("unknown option `{a}`")));
            }
            a => {
                if trace.replace(PathBuf::from(a)).is_some() {
                    return Err(usage("give one trace"));
                }
            }
        }
    }
    let trace = trace.ok_or_else(|| usage("no trace given"))?;
    let sizes = sizes.ok_or_else(|| usage("no cache sizes given (--sizes)"))?;
    let csv = |cols: Option<&str>| match cols {
        Some(spec) => CsvColumns::parse_spec(spec, delimiter, header).map_err(usage),
        None => Ok(CsvColumns {
            delimiter,
            header,
            ..CsvColumns::default()
        }),
    };
    let format = match format {
        Some("oracle") => Format::OracleGeneral,
        Some(_) => Format::Csv(csv(columns.as_deref())?),
        None => match Format::guess(&trace) {
            Format::Csv(_) => Format::Csv(csv(columns.as_deref())?),
            oracle => oracle,
        },
    };
    if matches!(format, Format::OracleGeneral) && (columns.is_some() || header || delimiter != b',')
    {
        return Err(usage(
            "--columns, --delimiter and --header are for CSV traces",
        ));
    }
    if trace.as_os_str() == "-" && sizes.iter().any(|(_, s)| matches!(s, Size::Share(_))) {
        return Err(usage(
            "sizes as a share of the footprint read the trace twice, which standard input cannot",
        ));
    }
    Ok(Some(Options {
        trace,
        format,
        sizes,
        policies,
        ignore_size,
        limit,
        threads: threads.max(1),
        output,
    }))
}

fn count(name: &str, v: &str) -> Result<u64, Error> {
    match v.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(usage(format!(
            "{name} takes a whole number above 0, not `{v}`"
        ))),
    }
}

fn parse_policies(list: &str) -> Result<Vec<&'static str>, Error> {
    let mut out: Vec<&'static str> = Vec::new();
    for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        let known = POLICIES
            .iter()
            .find(|p| p.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                usage(format!(
                    "unknown policy `{name}` (known: {})",
                    POLICIES.join(", ")
                ))
            })?;
        if !out.contains(known) {
            out.push(known);
        }
    }
    if out.is_empty() {
        return Err(usage("no policies given"));
    }
    Ok(out)
}

/// Parses `64MiB,1GiB,10%`.
pub fn parse_sizes(list: &str) -> Result<Vec<(String, Size)>, Error> {
    let mut out = Vec::new();
    for item in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let size =
            parse_size(item).ok_or_else(|| usage(format!("`{item}` is not a cache size")))?;
        out.push((item.to_string(), size));
    }
    if out.is_empty() {
        return Err(usage("no cache sizes given (--sizes)"));
    }
    Ok(out)
}

fn parse_size(s: &str) -> Option<Size> {
    if let Some(pct) = s.strip_suffix('%') {
        let p: f64 = pct.trim().parse().ok()?;
        return (p > 0.0 && p <= 100.0).then_some(Size::Share(p / 100.0));
    }
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num.parse().ok()?;
    let unit: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "t" | "tib" => 1 << 40,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        _ => return None,
    };
    let bytes = num * unit as f64;
    (bytes >= 1.0 && bytes < u64::MAX as f64).then_some(Size::Bytes(bytes as u64))
}

/// One row of the results.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub policy: &'static str,
    /// The size as given.
    pub size: String,
    pub cache_bytes: u64,
    #[serde(flatten)]
    pub stats: Stats,
    pub hit_ratio: f64,
    pub byte_hit_ratio: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub trace: String,
    pub requests: u64,
    pub elapsed_secs: f64,
    /// Present when a size was a share of it.
    pub footprint: Option<Footprint>,
    pub results: Vec<Row>,
}

fn open(opts: &Options) -> Result<Trace, Error> {
    let t = trace::open(&opts.trace, &opts.format)
        .map_err(|e| Error::Run(format!("{}: {e}", opts.trace.display())))?;
    let t: Trace = match opts.limit {
        Some(n) => Box::new(t.take(n as usize)),
        None => t,
    };
    Ok(if opts.ignore_size {
        Box::new(t.map(|r| r.map(|r| Request { size: 1, ..r })))
    } else {
        t
    })
}

/// Runs the simulations `opts` describe.
pub fn simulate(opts: &Options) -> Result<Report, Error> {
    let name = |p: &Path| p.display().to_string();
    let read_err = |e: io::Error| Error::Run(format!("{}: {e}", name(&opts.trace)));
    let footprint = if opts.sizes.iter().any(|(_, s)| matches!(s, Size::Share(_))) {
        Some(super::footprint(open(opts)?).map_err(read_err)?)
    } else {
        None
    };
    let mut sims = Vec::new();
    for &policy in &opts.policies {
        for (_, size) in &opts.sizes {
            let capacity = match *size {
                Size::Bytes(b) => b,
                Size::Share(s) => {
                    let total = footprint.map_or(0, |f| f.bytes);
                    ((total as f64 * s).round() as u64).max(1)
                }
            };
            let p = super::policy(policy, capacity).expect("policies are checked when parsed");
            sims.push(Simulation::new(p, capacity));
        }
    }
    let Replay { requests, elapsed } =
        super::replay(open(opts)?, &mut sims, opts.threads).map_err(read_err)?;
    let labels = opts.policies.iter().flat_map(|_| opts.sizes.iter());
    let results = sims
        .into_iter()
        .zip(labels)
        .map(|(sim, (label, _))| Row {
            policy: sim.policy.name(),
            size: label.clone(),
            cache_bytes: sim.capacity,
            hit_ratio: sim.stats.hit_ratio(),
            byte_hit_ratio: sim.stats.byte_hit_ratio(),
            stats: sim.stats,
        })
        .collect();
    Ok(Report {
        trace: name(&opts.trace),
        requests,
        elapsed_secs: elapsed.as_secs_f64(),
        footprint,
        results,
    })
}

/// Parses `args`, runs, and writes the report.
pub fn run(args: &[String], out: &mut dyn Write) -> Result<(), Error> {
    let Some(opts) = parse(args)? else {
        return writeln!(out, "{USAGE}").map_err(write_err);
    };
    let report = simulate(&opts)?;
    write_report(&report, opts.output, out).map_err(write_err)
}

fn write_err(e: io::Error) -> Error {
    Error::Run(format!("writing results: {e}"))
}

pub fn write_report(report: &Report, output: Output, out: &mut dyn Write) -> io::Result<()> {
    match output {
        Output::Json => {
            serde_json::to_writer_pretty(&mut *out, report)?;
            writeln!(out)
        }
        Output::Csv => {
            writeln!(
                out,
                "policy,size,cache_bytes,requests,hits,hit_ratio,bytes,hit_bytes,byte_hit_ratio,writes,write_bytes"
            )?;
            for r in &report.results {
                let s = &r.stats;
                writeln!(
                    out,
                    "{},{},{},{},{},{:.6},{},{},{:.6},{},{}",
                    r.policy,
                    r.size,
                    r.cache_bytes,
                    s.requests,
                    s.hits,
                    r.hit_ratio,
                    s.bytes,
                    s.hit_bytes,
                    r.byte_hit_ratio,
                    s.writes,
                    s.write_bytes
                )?;
            }
            Ok(())
        }
        Output::Table => write_table(report, out),
    }
}

fn write_table(report: &Report, out: &mut dyn Write) -> io::Result<()> {
    let rate = if report.elapsed_secs > 0.0 {
        report.requests as f64 / report.elapsed_secs / 1e6
    } else {
        0.0
    };
    writeln!(
        out,
        "{}: {} requests replayed through {} simulations in {:.2} s ({rate:.1} M requests/s)",
        report.trace,
        report.requests,
        report.results.len(),
        report.elapsed_secs,
    )?;
    if let Some(f) = report.footprint {
        writeln!(
            out,
            "footprint: {} objects, {} in {} requests",
            f.objects,
            human(f.bytes),
            f.requests
        )?;
    }
    let header = [
        "policy",
        "size",
        "cache",
        "hit ratio",
        "byte hit ratio",
        "writes",
        "written",
    ];
    let rows: Vec<[String; 7]> = report
        .results
        .iter()
        .map(|r| {
            [
                r.policy.to_string(),
                r.size.clone(),
                human(r.cache_bytes),
                format!("{:.4}", r.hit_ratio),
                format!("{:.4}", r.byte_hit_ratio),
                r.stats.writes.to_string(),
                human(r.stats.write_bytes),
            ]
        })
        .collect();
    let mut widths = header.map(str::len);
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.len());
        }
    }
    let line = |cells: &[&str]| {
        let mut s = String::new();
        for (i, (cell, w)) in cells.iter().zip(widths).enumerate() {
            if i > 0 {
                s.push_str("  ");
            }
            // Names left-aligned, numbers right-aligned.
            if i < 2 {
                s.push_str(&format!("{cell:<w$}"));
            } else {
                s.push_str(&format!("{cell:>w$}"));
            }
        }
        s.trim_end().to_string()
    };
    writeln!(out, "{}", line(&header))?;
    for row in &rows {
        let cells: Vec<&str> = row.iter().map(String::as_str).collect();
        writeln!(out, "{}", line(&cells))?;
    }
    Ok(())
}

/// Bytes in binary units, to three significant figures or so.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[unit])
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::trace::oracle_record;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| a.to_string()).collect()
    }

    fn opts(s: &[&str]) -> Options {
        parse(&args(s)).unwrap().unwrap()
    }

    fn usage_err(s: &[&str]) -> String {
        match parse(&args(s)) {
            Err(Error::Usage(why)) => why,
            other => panic!("{s:?} parsed as {other:?}"),
        }
    }

    #[test]
    fn sizes() {
        let got = parse_sizes("1000, 64KiB,1.5GiB,2gb,10%,0.5%,7k").unwrap();
        let sizes: Vec<Size> = got.iter().map(|(_, s)| *s).collect();
        assert_eq!(
            sizes,
            [
                Size::Bytes(1_000),
                Size::Bytes(64 << 10),
                Size::Bytes(3 << 29),
                Size::Bytes(2_000_000_000),
                Size::Share(0.1),
                Size::Share(0.005),
                Size::Bytes(7 << 10),
            ]
        );
        assert_eq!(got[1].0, "64KiB");
        for bad in [
            "",
            "0",
            "0%",
            "101%",
            "x%",
            "12 parsecs",
            "MiB",
            "0.1",
            "1e30TiB",
        ] {
            assert!(parse_sizes(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn defaults_and_every_option() {
        let o = opts(&["-s", "1MiB", "t.oracleGeneral.zst"]);
        assert_eq!(o.format, Format::OracleGeneral);
        assert_eq!(o.policies, POLICIES);
        assert_eq!(o.output, Output::Table);
        assert!(o.threads >= 1);
        assert_eq!((o.limit, o.ignore_size), (None, false));

        let o = opts(&[
            "t.txt",
            "--sizes",
            "1MiB,2MiB",
            "--policies",
            "LRU, lru,s3fifo",
            "--format",
            "csv",
            "--columns",
            "id=1",
            "--delimiter",
            "tab",
            "--header",
            "--ignore-size",
            "--limit",
            "10",
            "--threads",
            "3",
            "--output",
            "json",
        ]);
        assert_eq!(o.policies, ["lru", "s3fifo"]);
        assert_eq!(
            o.format,
            Format::Csv(CsvColumns {
                delimiter: b'\t',
                time: None,
                id: 0,
                size: None,
                header: true,
            })
        );
        assert_eq!((o.limit, o.threads, o.ignore_size), (Some(10), 3, true));
        assert_eq!(o.output, Output::Json);

        let o = opts(&["-s", "1", "--delimiter", ";", "a.csv", "--output", "csv"]);
        assert_eq!(
            o.format,
            Format::Csv(CsvColumns {
                delimiter: b';',
                ..CsvColumns::default()
            })
        );
        assert_eq!(o.output, Output::Csv);
        let o = opts(&["-s", "1", "--format", "oracleGeneral", "a.csv"]);
        assert_eq!(o.format, Format::OracleGeneral);
        let o = opts(&["-s", "1", "--format", "CSV", "a", "--delimiter", "space"]);
        assert!(matches!(
            o.format,
            Format::Csv(CsvColumns {
                delimiter: b' ',
                ..
            })
        ));
        assert!(parse(&args(&["--help"])).unwrap().is_none());
    }

    #[test]
    fn command_line_mistakes() {
        let cases: &[(&[&str], &str)] = &[
            (&["-s", "1"], "no trace given"),
            (&["t"], "no cache sizes given (--sizes)"),
            (&["t", "-s"], "-s needs a value"),
            (&["t", "-s", ","], "no cache sizes given (--sizes)"),
            (&["t", "u", "-s", "1"], "give one trace"),
            (&["t", "-s", "1", "--bogus"], "unknown option `--bogus`"),
            (
                &["t", "-s", "1", "-p", "clock"],
                "unknown policy `clock` (known: s3fifo, lru, sieve, wtinylfu)",
            ),
            (&["t", "-s", "1", "-p", ","], "no policies given"),
            (
                &["t", "-s", "1", "--format", "parquet"],
                "unknown format `parquet`",
            ),
            (&["t", "-s", "1", "--output", "xml"], "unknown output `xml`"),
            (
                &["t", "-s", "1", "--delimiter", "::"],
                "delimiter `::` is not one character",
            ),
            (
                &["t", "-s", "1", "--limit", "0"],
                "--limit takes a whole number above 0, not `0`",
            ),
            (
                &["t", "-s", "1", "--threads", "x"],
                "--threads takes a whole number above 0, not `x`",
            ),
            (
                &["t.csv", "-s", "1", "--columns", "size=2"],
                "the columns must name id",
            ),
            (
                &["t", "-s", "1", "--header"],
                "--columns, --delimiter and --header are for CSV traces",
            ),
            (
                &["-", "-s", "5%"],
                "sizes as a share of the footprint read the trace twice, which standard input cannot",
            ),
        ];
        for (a, want) in cases {
            assert_eq!(usage_err(a), *want, "{a:?}");
        }
        let o = opts(&["-", "-s", "5MiB"]);
        assert_eq!(o.trace, PathBuf::from("-"));
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1023), "1023 B");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(200 << 20), "200 MiB");
        assert_eq!(human(3 << 40), "3.0 TiB");
        assert_eq!(human(5000 << 40), "5000 TiB");
    }

    fn write_trace(name: &str, reqs: &[Request]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("edgefront-sim-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let bytes: Vec<u8> = reqs.iter().flat_map(oracle_record).collect();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn workload() -> Vec<Request> {
        (0..20_000u64)
            .map(|i| {
                let id = if i % 2 == 0 { i % 40 } else { 40 + i % 5_000 };
                Request {
                    time: i,
                    id,
                    size: 100 + id % 900,
                }
            })
            .collect()
    }

    fn run_str(a: &[String]) -> (ExitCode, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = main(a, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn runs_and_reports_every_pair_in_every_output() {
        let reqs = workload();
        let path = write_trace("w.oracleGeneral", &reqs);
        let p = path.to_str().unwrap();

        let (code, out, err) = run_str(&args(&["-s", "64KiB,1%,50%", p, "--output", "json"]));
        assert_eq!(code, ExitCode::SUCCESS, "{err}");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["requests"], 20_000);
        let footprint_bytes = v["footprint"]["bytes"].as_u64().unwrap();
        assert_eq!(v["footprint"]["objects"], 2_520);
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 12);
        for (r, (policy, size)) in results.iter().zip(
            ["s3fifo", "lru", "sieve", "wtinylfu"]
                .iter()
                .flat_map(|p| ["64KiB", "1%", "50%"].map(|s| (*p, s))),
        ) {
            assert_eq!(r["policy"], policy);
            assert_eq!(r["size"], size);
            assert_eq!(r["requests"], 20_000);
            let hits = r["hits"].as_u64().unwrap();
            let ratio = r["hit_ratio"].as_f64().unwrap();
            assert!((ratio - hits as f64 / 20_000.0).abs() < 1e-12);
            // Every miss fits, so every miss is written, except those
            // W-TinyLFU's admission turns away.
            let writes = r["writes"].as_u64().unwrap();
            if policy == "wtinylfu" {
                assert!(writes <= 20_000 - hits, "{r}");
            } else {
                assert_eq!(writes, 20_000 - hits);
            }
        }
        assert_eq!(
            results[1]["cache_bytes"].as_u64().unwrap(),
            (footprint_bytes as f64 * 0.01).round() as u64
        );
        assert_eq!(
            results[2]["cache_bytes"].as_u64().unwrap(),
            footprint_bytes / 2
        );
        // The hot half of the trace is held at half the footprint.
        assert!(results[2]["hit_ratio"].as_f64().unwrap() > 0.45);
        let bigger =
            |a: usize, b: usize| results[a]["hits"].as_u64() <= results[b]["hits"].as_u64();
        assert!(bigger(1, 2) && bigger(4, 5) && bigger(7, 8) && bigger(10, 11));

        let (code, out, _) = run_str(&args(&["-s", "1MiB", p, "--output", "csv", "-p", "lru"]));
        assert_eq!(code, ExitCode::SUCCESS);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("policy,size,cache_bytes,requests,hits,"));
        assert!(
            lines[1].starts_with("lru,1MiB,1048576,20000,"),
            "{}",
            lines[1]
        );

        let (code, out, _) = run_str(&args(&[
            "-s",
            "1MiB,10%",
            p,
            "--limit",
            "1000",
            "--ignore-size",
        ]));
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(
            out.contains(": 1000 requests replayed through 8 simulations in "),
            "{out}"
        );
        assert!(
            out.contains("footprint: 520 objects, 520 B in 1000 requests"),
            "{out}"
        );
        let table: Vec<&str> = out.lines().skip(2).collect();
        assert_eq!(
            table[0],
            "policy    size    cache  hit ratio  byte hit ratio  writes  written"
        );
        assert_eq!(table.len(), 9);
        assert!(
            table[1].starts_with("s3fifo    1MiB  1.0 MiB "),
            "{}",
            table[1]
        );
        assert!(
            table[2].starts_with("s3fifo    10%      52 B "),
            "{}",
            table[2]
        );
        // At 52 B every policy hits the same 48%, but W-TinyLFU writes
        // only the objects it admits.
        assert_eq!(
            table[8],
            "wtinylfu  10%      52 B     0.4800          0.4800      84     84 B"
        );
    }

    #[test]
    fn help_and_failures_exit_codes() {
        let (code, out, _) = run_str(&args(&["-h"]));
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(out.starts_with("usage: cache_sim"));

        let (code, _, err) = run_str(&args(&["-s", "1"]));
        assert_eq!(code, ExitCode::from(2));
        assert!(err.starts_with("cache_sim: no trace given\n\nusage:"));

        let (code, _, err) = run_str(&args(&["-s", "1", "/nonexistent/trace.bin"]));
        assert_eq!(code, ExitCode::FAILURE);
        assert!(
            err.starts_with("cache_sim: /nonexistent/trace.bin: "),
            "{err}"
        );

        let mut bytes: Vec<u8> = workload().iter().take(3).flat_map(oracle_record).collect();
        bytes.truncate(bytes.len() - 1);
        let path = write_trace("short.oracleGeneral", &[]);
        std::fs::write(&path, bytes).unwrap();
        let p = path.to_str().unwrap();
        for sizes in ["1MiB", "10%"] {
            let (code, _, err) = run_str(&args(&["-s", sizes, p]));
            assert_eq!(code, ExitCode::FAILURE);
            assert!(
                err.ends_with("record 3 is truncated: 23 of 24 bytes\n"),
                "{err}"
            );
        }

        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let path = write_trace("ok.oracleGeneral", &workload()[..10]);
        for output in ["table", "csv", "json"] {
            let a = args(&["-s", "1MiB", path.to_str().unwrap(), "--output", output]);
            let err = run(&a, &mut Closed).unwrap_err();
            assert!(
                matches!(err, Error::Run(ref w) if w.starts_with("writing results: ")),
                "{err:?}"
            );
        }
        assert!(run(&args(&["-h"]), &mut Closed).is_err());
    }

    #[test]
    fn a_table_of_nothing_replayed() {
        let report = Report {
            trace: "empty".into(),
            requests: 0,
            elapsed_secs: 0.0,
            footprint: None,
            results: Vec::new(),
        };
        let mut out = Vec::new();
        write_report(&report, Output::Table, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with(
            "empty: 0 requests replayed through 0 simulations in 0.00 s (0.0 M requests/s)\n"
        ));
    }
}
