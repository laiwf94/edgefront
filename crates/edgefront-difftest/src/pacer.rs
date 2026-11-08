//! Diffs edgefront-limiter against libpacer.
//!
//! `legacy/pacer/tests/diffdrive.c` reads one operation per line on stdin,
//! applies it to libpacer and prints exactly one line per operation; its
//! header documents the protocol. [`Interpreter`] is the same protocol
//! served by edgefront-limiter, written to print byte-identical lines, so
//! the two can be compared line by line with no FFI.
//!
//! [`run`] generates seeded scripts of random operations (see
//! [`generate`]), feeds each one to the C driver and to an [`Interpreter`],
//! and stops at the first line where the transcripts differ. That line is
//! reported with the script's seed, the step number, the operation, both
//! answers and the steps leading up to it, so a divergence found in CI can
//! be replayed from the seed alone. Every GCRA and window line ends with
//! the object's raw state and every script ends with a dump of each live
//! table, so not only the decisions but the retry times and the table
//! contents are compared after every step.
//!
//! Two differences between the implementations are deliberate, and the
//! generator stays clear of both:
//!
//! - libpacer computes a rate's tolerance as interval times burst in
//!   wrapping 128-bit arithmetic; edgefront-limiter rejects a product that
//!   overflows. Generated rates never overflow.
//! - libpacer's window compares `now < head_start + slot_ns`, which wraps
//!   for a window near the top of the clock; edgefront-limiter compares the
//!   difference. The generated clock stays below [`generate::CLOCK_MAX`].
//!
//! One more difference is reconciled here rather than avoided: libpacer
//! clamps a lone limiter's timestamps at 2^57 ns, where its absolute
//! arrival times run out, while `Gcra` keeps them in a moving epoch and has
//! no horizon. The interpreter applies libpacer's clamp before calling
//! `Gcra`, so the generated clock still crosses it. (The keyed table keeps
//! the clamp itself.)

pub mod generate;

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use edgefront_limiter::gcra::MAX_NOW_NS;
use edgefront_limiter::table::KEY_MAX;
use edgefront_limiter::{
    Decision, Error, FullPolicy, Gcra, KeyHash, Rate, Table, TableConfig, Window,
};

use crate::driver::{self, Transcript};
pub use generate::Script;

/// Objects are numbered 0 to 15 per kind, as in the C driver.
pub const MAX_OBJS: usize = 16;
/// The longest key the driver accepts, longer than [`KEY_MAX`] so that
/// oversized keys can be sent.
const MAX_HEX_KEY: usize = 64;
/// The most words in one line.
const MAX_TOKENS: usize = 12;
/// The longest line the driver accepts, without its newline.
pub const MAX_LINE: usize = 4094;
/// How many steps before a divergence its report shows.
const CONTEXT: usize = 8;

/// A malformed line: a bug in whatever wrote the script, not a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptError {
    /// The 1-based line number.
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ScriptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "diffdrive: line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ScriptError {}

type Parsed<T> = Result<T, String>;

/// The diffdrive protocol served by edgefront-limiter.
#[derive(Debug)]
pub struct Interpreter {
    line_no: usize,
    now: u64,
    gcras: [Option<Gcra>; MAX_OBJS],
    tables: [Option<Table>; MAX_OBJS],
    windows: [Option<Window>; MAX_OBJS],
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

impl Interpreter {
    /// A fresh driver: the clock at 0 and no objects.
    pub fn new() -> Self {
        Self {
            line_no: 0,
            now: 0,
            gcras: Default::default(),
            tables: Default::default(),
            windows: Default::default(),
        }
    }

    /// Run one line, without its newline. Returns the line to print, or
    /// `None` for a blank line or a comment.
    pub fn line(&mut self, line: &str) -> Result<Option<String>, ScriptError> {
        self.line_no += 1;
        self.run_line(line).map_err(|message| ScriptError {
            line: self.line_no,
            message,
        })
    }

    fn run_line(&mut self, line: &str) -> Parsed<Option<String>> {
        if line.len() > MAX_LINE {
            return Err(format!("line longer than {MAX_LINE} bytes"));
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        let line = line.trim_start_matches([' ', '\t']);
        if line.is_empty() || line.starts_with('#') {
            return Ok(None);
        }
        if let Some(rest) = line.strip_prefix("echo") {
            if rest.is_empty() {
                return Ok(Some(String::new()));
            }
            if let Some(text) = rest.strip_prefix(' ') {
                return Ok(Some(text.to_owned()));
            }
        }
        let argv: Vec<&str> = line.split([' ', '\t']).filter(|w| !w.is_empty()).collect();
        if argv.len() > MAX_TOKENS {
            return Err(format!("more than {MAX_TOKENS} words"));
        }
        let out = match argv[0] {
            "now" => {
                need(&argv, 2, "now N")?;
                self.now = parse_u64(argv[1])?;
                format!("now={}", self.now)
            }
            "advance" => {
                need(&argv, 2, "advance N")?;
                self.now = self.now.saturating_add(parse_u64(argv[1])?);
                format!("now={}", self.now)
            }
            "sync" => {
                need(&argv, 1, "sync")?;
                "sync".to_owned()
            }
            "gcra" => self.gcra(&argv)?,
            "table" => self.table(&argv)?,
            "window" => self.window(&argv)?,
            cmd => return Err(format!("unknown command {cmd}")),
        };
        Ok(Some(out))
    }

    fn gcra(&mut self, argv: &[&str]) -> Parsed<String> {
        if argv.len() < 3 {
            return Err("usage: gcra I OP ...".into());
        }
        let i = parse_index(argv[1])?;
        // libpacer clamps a lone limiter's clock at 2^57 ns; edgefront's
        // `Gcra` has no horizon, so the clamp is applied here, as the C
        // driver's library applies it.
        let (op, now) = (argv[2], self.now.min(MAX_NOW_NS - 1));
        let mut out = String::new();
        if op == "init" {
            need(argv, 6, "gcra I init T P B")?;
            let rate = parse_rate(&argv[3..6])?;
            match Gcra::new(&rate) {
                Ok(g) => {
                    self.gcras[i] = Some(g);
                    out.push_str("rc=0");
                }
                Err(e) => rc_err(&mut out, e),
            }
            if let Some(g) = &self.gcras[i] {
                gcra_state(&mut out, g);
            }
            return Ok(out);
        }
        let g = self.gcras[i]
            .as_ref()
            .ok_or_else(|| format!("gcra {i} is not initialised"))?;
        match op {
            "take" => {
                need(argv, 4, "gcra I take COST")?;
                match g.take(now, parse_u64(argv[3])?) {
                    Ok(d) => allowed(&mut out, &d),
                    Err(e) => denied(&mut out, e),
                }
            }
            "take_upto" => {
                need(argv, 4, "gcra I take_upto MAX")?;
                got(&mut out, &g.take_upto(now, parse_u64(argv[3])?));
            }
            "take_range" => {
                need(argv, 5, "gcra I take_range MIN MAX")?;
                let (min, max) = (parse_u64(argv[3])?, parse_u64(argv[4])?);
                got(&mut out, &g.take_range(now, min, max));
            }
            "refund" => {
                need(argv, 4, "gcra I refund N")?;
                g.refund(now, parse_u64(argv[3])?);
                out.push_str("ok");
            }
            "remaining" => {
                need(argv, 3, "gcra I remaining")?;
                let _ = write!(out, "remaining={}", g.remaining(now));
            }
            "reconfigure" => {
                need(argv, 6, "gcra I reconfigure T P B")?;
                rc_unit(&mut out, g.reconfigure(&parse_rate(&argv[3..6])?));
            }
            "state" => {
                need(argv, 3, "gcra I state")?;
                out.push_str("ok");
            }
            _ => return Err(format!("unknown gcra op {op}")),
        }
        gcra_state(&mut out, g);
        Ok(out)
    }

    fn table(&mut self, argv: &[&str]) -> Parsed<String> {
        if argv.len() < 3 {
            return Err("usage: table I OP ...".into());
        }
        let i = parse_index(argv[1])?;
        let (op, now) = (argv[2], self.now);
        let mut out = String::new();
        if op == "create" {
            need(
                argv,
                10,
                "table I create MAX SHARDS T P B deny|evict HASHKEY",
            )?;
            if self.tables[i].is_some() {
                return Err(format!("table {i} already exists"));
            }
            let full_policy = match argv[8] {
                "deny" => FullPolicy::Deny,
                "evict" => FullPolicy::Evict,
                p => return Err(format!("unknown full policy {p}")),
            };
            let hash_key: [u8; 16] = parse_hex(argv[9], 16)?
                .try_into()
                .map_err(|_| "hash key must be 32 hex digits".to_owned())?;
            let cfg = TableConfig {
                max_entries: parse_u32(argv[3])?,
                shards: parse_u32(argv[4])?,
                rate: parse_rate(&argv[5..8])?,
                hash_key: Some(hash_key),
                full_policy,
                key_hash: KeyHash::Sip24,
            };
            match Table::new(&cfg) {
                Ok(t) => {
                    self.tables[i] = Some(t);
                    out.push_str("rc=0");
                }
                Err(e) => rc_err(&mut out, e),
            }
            return Ok(out);
        }
        let Some(t) = self.tables[i].as_ref() else {
            return Err(format!("table {i} does not exist"));
        };
        match op {
            "reconfigure" => {
                need(argv, 6, "table I reconfigure T P B")?;
                rc_unit(&mut out, t.reconfigure(&parse_rate(&argv[3..6])?));
            }
            "sweep" => {
                need(argv, 4, "table I sweep NSHARDS")?;
                let _ = write!(out, "swept={}", t.sweep(now, parse_u32(argv[3])?));
            }
            "stats" => {
                need(argv, 3, "table I stats")?;
                let s = t.stats();
                let _ = write!(
                    out,
                    "entries={} capacity={} inserts={} removals={} reclaims={} evictions={} \
                     rejections={} holds={}",
                    s.entries,
                    s.capacity,
                    s.inserts,
                    s.removals,
                    s.reclaims,
                    s.evictions,
                    s.rejections,
                    s.holds
                );
            }
            "check" => {
                need(argv, 3, "table I check")?;
                match t.check_invariants() {
                    Ok(()) => out.push_str("check=0"),
                    Err(e) => {
                        let _ = write!(out, "check=failed ({e})");
                    }
                }
            }
            "destroy" => {
                need(argv, 3, "table I destroy")?;
                self.tables[i] = None;
                out.push_str("ok");
            }
            _ => table_keyed(t, now, op, argv, &mut out)?,
        }
        Ok(out)
    }

    fn window(&mut self, argv: &[&str]) -> Parsed<String> {
        if argv.len() < 3 {
            return Err("usage: window I OP ...".into());
        }
        let i = parse_index(argv[1])?;
        let (op, now) = (argv[2], self.now);
        let mut out = String::new();
        if op == "init" {
            need(argv, 5, "window I init WINDOW_NS SLOTS")?;
            let (window_ns, slots) = (parse_u64(argv[3])?, parse_u32(argv[4])?);
            match Window::new(window_ns, slots, now) {
                Ok(w) => {
                    self.windows[i] = Some(w);
                    out.push_str("rc=0");
                }
                Err(e) => rc_err(&mut out, e),
            }
            if let Some(w) = &self.windows[i] {
                window_state(&mut out, w);
            }
            return Ok(out);
        }
        let w = self.windows[i]
            .as_mut()
            .ok_or_else(|| format!("window {i} is not initialised"))?;
        match op {
            "add" => {
                need(argv, 4, "window I add N")?;
                w.add(now, parse_u64(argv[3])?);
                out.push_str("ok");
            }
            "count" => {
                need(argv, 3, "window I count")?;
                let _ = write!(out, "count={}", w.count(now));
            }
            _ => return Err(format!("unknown window op {op}")),
        }
        window_state(&mut out, w);
        Ok(out)
    }
}

fn table_keyed(t: &Table, now: u64, op: &str, argv: &[&str], out: &mut String) -> Parsed<()> {
    if argv.len() < 4 {
        return Err(format!("usage: table I {op} KEY ..."));
    }
    let key = parse_hex(argv[3], MAX_HEX_KEY)?;
    let key = key.as_slice();
    match op {
        "take" => {
            need(argv, 5, "table I take KEY COST")?;
            match t.take(key, now, parse_u64(argv[4])?) {
                Ok(d) => allowed(out, &d),
                Err(e) => denied(out, e),
            }
        }
        "take_upto" => {
            need(argv, 5, "table I take_upto KEY MAX")?;
            ok_decision(out, t.take_upto(key, now, parse_u64(argv[4])?));
        }
        "take_range" => {
            need(argv, 6, "table I take_range KEY MIN MAX")?;
            let (min, max) = (parse_u64(argv[4])?, parse_u64(argv[5])?);
            ok_decision(out, t.take_range(key, now, min, max));
        }
        "refund" => {
            need(argv, 5, "table I refund KEY N")?;
            t.refund(key, now, parse_u64(argv[4])?);
            out.push_str("ok");
        }
        "hold" => {
            need(argv, 5, "table I hold KEY MAX_HOLDS")?;
            match t.hold(key, now, parse_u32(argv[4])?) {
                Ok(d) => allowed(out, &d),
                Err(e) => denied(out, e),
            }
        }
        "release" => {
            need(argv, 4, "table I release KEY")?;
            rc_unit(out, t.release(key));
        }
        "remove" => {
            need(argv, 4, "table I remove KEY")?;
            rc_unit(out, t.remove(key));
        }
        "get" => {
            need(argv, 4, "table I get KEY")?;
            if key.is_empty() || key.len() > KEY_MAX {
                rc_err(out, Error::InvalidKey);
            } else if let Some(e) = t.get(key) {
                let _ = write!(out, "tat={} holds={}", e.tat, e.holds);
            } else {
                out.push_str("absent");
            }
        }
        _ => return Err(format!("unknown table op {op}")),
    }
    Ok(())
}

/// libpacer's name for the error code an [`Error`] stands for.
pub fn err_name(e: Error) -> &'static str {
    match e {
        Error::CostExceedsBurst => "ERANGE",
        Error::TableFull => "EFULL",
        Error::NoSuchKey => "ENOENT",
        Error::HoldLimit => "ELIMIT",
        // InvalidRate, InvalidConfig, InvalidKey, InvalidWindow, and any
        // added later until it is given its own code.
        _ => "EINVAL",
    }
}

fn rc_err(out: &mut String, e: Error) {
    let _ = write!(out, "rc={}", err_name(e));
}

fn rc_unit(out: &mut String, r: Result<(), Error>) {
    match r {
        Ok(()) => out.push_str("rc=0"),
        Err(e) => rc_err(out, e),
    }
}

fn decision(out: &mut String, d: &Decision) {
    let _ = write!(
        out,
        " granted={} remaining={} retry={}",
        d.granted, d.remaining, d.retry_after_ns
    );
}

/// A take whose return code is whether it was admitted.
fn allowed(out: &mut String, d: &Decision) {
    let _ = write!(out, "rc={}", u8::from(d.allowed));
    decision(out, d);
}

/// A call that failed: libpacer zeroes the decision first.
fn denied(out: &mut String, e: Error) {
    rc_err(out, e);
    decision(out, &Decision::default());
}

/// A range take on a table, which returns 0 whatever it granted.
fn ok_decision(out: &mut String, r: Result<Decision, Error>) {
    match r {
        Ok(d) => {
            out.push_str("rc=0");
            decision(out, &d);
        }
        Err(e) => denied(out, e),
    }
}

/// A range take on a lone limiter, which returns the grant.
fn got(out: &mut String, d: &Decision) {
    let _ = write!(out, "got={}", d.granted);
    decision(out, d);
}

fn gcra_state(out: &mut String, g: &Gcra) {
    let _ = write!(
        out,
        " tat={} iv={} tol={}",
        g.tat(),
        g.interval(),
        g.tolerance()
    );
}

fn window_state(out: &mut String, w: &Window) {
    let _ = write!(out, " start={} total={}", w.head_start(), w.total());
}

fn need(argv: &[&str], want: usize, usage: &str) -> Parsed<()> {
    if argv.len() == want {
        Ok(())
    } else {
        Err(format!("usage: {usage}"))
    }
}

/// A decimal u64 that starts with a digit, as the driver's `strtoull`
/// check accepts.
fn parse_u64(s: &str) -> Parsed<u64> {
    if !s.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(format!("not a number: {s}"));
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("not a u64: {s}"));
    }
    s.parse().map_err(|_| format!("not a u64: {s}"))
}

fn parse_u32(s: &str) -> Parsed<u32> {
    u32::try_from(parse_u64(s)?).map_err(|_| format!("not a u32: {s}"))
}

fn parse_index(s: &str) -> Parsed<usize> {
    match parse_u64(s)? {
        i if i < MAX_OBJS as u64 => Ok(i as usize),
        _ => Err(format!("object index {s} out of range")),
    }
}

fn parse_rate(argv: &[&str]) -> Parsed<Rate> {
    Ok(Rate::new(
        parse_u64(argv[0])?,
        parse_u64(argv[1])?,
        parse_u64(argv[2])?,
    ))
}

/// Hex of at most `cap` bytes, or `-` for the empty string.
fn parse_hex(s: &str, cap: usize) -> Parsed<Vec<u8>> {
    if s == "-" {
        return Ok(Vec::new());
    }
    if !s.len().is_multiple_of(2) || s.len() / 2 > cap {
        return Err(format!("bad hex of {} digits: {s}", s.len()));
    }
    let digit = |b: u8| (b as char).to_digit(16);
    s.as_bytes()
        .chunks(2)
        .map(|p| match (digit(p[0]), digit(p[1])) {
            (Some(hi), Some(lo)) => Ok((hi << 4 | lo) as u8),
            _ => Err(format!("bad hex: {s}")),
        })
        .collect()
}

/// Hex for a key, or `-` for the empty key.
pub fn hex_key(key: &[u8]) -> String {
    if key.is_empty() {
        return "-".to_owned();
    }
    key.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Every line `script` prints, from edgefront-limiter.
pub fn transcript(script: &str) -> Result<Vec<String>, ScriptError> {
    let mut it = Interpreter::new();
    let mut out = Vec::new();
    for line in script.lines() {
        out.extend(it.line(line)?);
    }
    Ok(out)
}

/// Serves the protocol from edgefront-limiter on `input` and `output`, the
/// Rust twin of the C driver. A malformed line is returned as an error
/// after the answers to the lines before it are written; input that is not
/// UTF-8 is malformed too.
pub fn serve(input: impl BufRead, mut output: impl Write) -> io::Result<Result<(), ScriptError>> {
    let mut it = Interpreter::new();
    for (n, line) in input.split(b'\n').enumerate() {
        let line = line?;
        let result = match std::str::from_utf8(&line) {
            Ok(text) => it.line(text),
            Err(_) => Err(ScriptError {
                line: n + 1,
                message: "not UTF-8".into(),
            }),
        };
        match result {
            Ok(Some(answer)) => writeln!(output, "{answer}")?,
            Ok(None) => {}
            Err(e) => {
                output.flush()?;
                return Ok(Err(e));
            }
        }
    }
    output.flush()?;
    Ok(Ok(()))
}

/// The program that plays libpacer: the C driver, or a stand-in in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl DriverCommand {
    /// The C driver this crate's build script compiled.
    pub fn c_driver() -> Self {
        Self {
            program: PathBuf::from(crate::PACER_DIFFDRIVE),
            args: Vec::new(),
        }
    }

    fn run(&self, script: &str) -> io::Result<Transcript> {
        driver::run_with_args(&self.program, &self.args, script)
    }
}

/// How much to generate, and from which seed. Script `i` is generated from
/// seed `seed + i` (wrapping), so any one script can be replayed alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub scripts: usize,
    /// Random operations per script, before the closing state dump.
    pub ops: usize,
    pub seed: u64,
}

impl Plan {
    /// The seed of script `i`.
    pub fn script_seed(&self, i: usize) -> u64 {
        self.seed.wrapping_add(i as u64)
    }
}

/// Where the transcripts first differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// The seed of the script, which regenerates it alone.
    pub seed: u64,
    /// The random operations in the script, for the replay command.
    pub ops: usize,
    /// The 1-based step: the operation's line in the script.
    pub step: usize,
    pub op: String,
    /// What the C driver printed, or `None` if it printed nothing.
    pub c: Option<String>,
    /// What edgefront-limiter printed, or `None` if it printed nothing.
    pub rust: Option<String>,
    /// Why the line counts as a divergence when both printed the same.
    pub note: Option<String>,
    /// The steps before it, with what both printed for them.
    pub before: Vec<(usize, String, String)>,
    /// The C driver's exit status and stderr, when it failed.
    pub driver_failure: Option<String>,
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |s: &Option<String>| s.clone().unwrap_or_else(|| "(nothing)".into());
        writeln!(f, "first divergence: seed {} step {}", self.seed, self.step)?;
        if !self.before.is_empty() {
            writeln!(f, "steps before it:")?;
            for (step, op, answer) in &self.before {
                writeln!(f, "  {step:>7}  {op}\n           -> {answer}")?;
            }
        }
        writeln!(f, "  {:>7}  {}", self.step, self.op)?;
        writeln!(f, "     C    -> {}", show(&self.c))?;
        writeln!(f, "     Rust -> {}", show(&self.rust))?;
        if let Some(note) = &self.note {
            writeln!(f, "  {note}")?;
        }
        if let Some(failure) = &self.driver_failure {
            writeln!(f, "  the C driver failed: {failure}")?;
        }
        write!(
            f,
            "replay: edgefront-difftest pacer --seed {} --scripts 1 --ops {}\n\
             script: edgefront-difftest pacer-script --seed {} --ops {}",
            self.seed, self.ops, self.seed, self.ops
        )
    }
}

/// What a run compared, and the first place the implementations disagreed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub seed: u64,
    /// Scripts run to the end or to a divergence.
    pub scripts: usize,
    /// Operations whose answers were compared.
    pub ops: usize,
    /// Answers by operation and return code, such as `table take rc=EFULL`.
    pub outcomes: BTreeMap<String, usize>,
    pub divergence: Option<Divergence>,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "seed {}", self.seed)?;
        writeln!(f, "scripts: {}", self.scripts)?;
        let outcomes: Vec<String> = self
            .outcomes
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect();
        writeln!(f, "operations: {} ({})", self.ops, outcomes.join(", "))?;
        match &self.divergence {
            None => write!(f, "divergences: 0"),
            Some(d) => write!(f, "divergences: 1\n\n{d}"),
        }
    }
}

/// The key an answer is counted under: the command, its operation, and
/// the return code when there is one.
fn outcome(op: &str, answer: &str) -> String {
    let mut words = op.split(' ');
    let cmd = words.next().unwrap_or_default();
    let name = match cmd {
        "gcra" | "table" | "window" => {
            let _index = words.next();
            format!("{cmd} {}", words.next().unwrap_or_default())
        }
        _ => cmd.to_owned(),
    };
    let first = answer.split(' ').next().unwrap_or_default();
    if first.starts_with("rc=") || first == "absent" || first.starts_with("check=") {
        format!("{name} {first}")
    } else if first.starts_with("tat=") {
        format!("{name} present")
    } else {
        name
    }
}

/// Compares the answers to `ops` from the C driver and from Rust, and
/// returns the first step where they differ, or where a table's invariant
/// check failed on either side.
pub fn compare(
    seed: u64,
    plan_ops: usize,
    ops: &[String],
    c: &Transcript,
    rust: &[String],
) -> Option<Divergence> {
    let c_lines: Vec<&str> = c.lines().collect();
    let failure = (!c.success()).then(|| format!("{}: {}", c.status, c.stderr.trim_end()));
    for (n, op) in ops.iter().enumerate() {
        let (theirs, ours) = (c_lines.get(n).copied(), rust.get(n).map(String::as_str));
        let note = match (theirs, ours) {
            (Some(a), Some(b)) if a != b => None,
            (Some(a), Some(_)) if a.starts_with("check=") && a != "check=0" => {
                Some("the table invariants do not hold".to_owned())
            }
            (Some(_), Some(_)) => continue,
            _ => None,
        };
        let start = n.saturating_sub(CONTEXT);
        return Some(Divergence {
            seed,
            ops: plan_ops,
            step: n + 1,
            op: op.clone(),
            c: theirs.map(str::to_owned),
            rust: ours.map(str::to_owned),
            note,
            before: (start..n)
                .map(|k| (k + 1, ops[k].clone(), rust[k].clone()))
                .collect(),
            driver_failure: failure,
        });
    }
    if c_lines.len() > ops.len() {
        return Some(Divergence {
            seed,
            ops: plan_ops,
            step: ops.len() + 1,
            op: "(end of script)".into(),
            c: Some(c_lines[ops.len()].to_owned()),
            rust: None,
            note: Some("the C driver printed more lines than there are operations".into()),
            before: Vec::new(),
            driver_failure: failure,
        });
    }
    if let Some(failure) = failure {
        return Some(Divergence {
            seed,
            ops: plan_ops,
            step: ops.len(),
            op: ops.last().cloned().unwrap_or_default(),
            c: c_lines.last().map(|s| (*s).to_owned()),
            rust: rust.last().cloned(),
            note: Some("every answer matched, but the C driver did not exit cleanly".into()),
            before: Vec::new(),
            driver_failure: Some(failure),
        });
    }
    None
}

/// Generates `plan.scripts` scripts, runs each through `driver` and
/// through edgefront-limiter, and stops at the first divergence. An error
/// means the driver could not be run at all, or the generator wrote a line
/// the Rust side rejects.
pub fn run(driver: &DriverCommand, plan: Plan) -> io::Result<Report> {
    let mut report = Report {
        seed: plan.seed,
        ..Report::default()
    };
    for i in 0..plan.scripts {
        let seed = plan.script_seed(i);
        let script = Script::generate(seed, plan.ops);
        let ours = transcript(&script.text())
            .map_err(|e| io::Error::other(format!("seed {seed}: generated a bad line: {e}")))?;
        let theirs = driver.run(&script.text())?;
        report.scripts += 1;
        if let Some(d) = compare(seed, plan.ops, &script.lines, &theirs, &ours) {
            report.ops += d.step - 1;
            report.divergence = Some(d);
            return Ok(report);
        }
        report.ops += ours.len();
        for (op, answer) in script.lines.iter().zip(&ours) {
            *report.outcomes.entry(outcome(op, answer)).or_default() += 1;
        }
    }
    Ok(report)
}

/// [`run`] against the C driver this crate built.
pub fn run_c(plan: Plan) -> io::Result<Report> {
    run(&DriverCommand::c_driver(), plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(it: &mut Interpreter, line: &str) -> String {
        it.line(line).unwrap().unwrap()
    }

    fn bad(line: &str) -> String {
        let mut it = Interpreter::new();
        it.line("gcra 0 init 1 1000 1").unwrap();
        it.line("table 0 create 4 1 1 1000 1 deny 000102030405060708090a0b0c0d0e0f")
            .unwrap();
        it.line("window 0 init 100 4").unwrap();
        it.line(line).unwrap_err().message
    }

    #[test]
    fn malformed_lines_are_errors_naming_the_line() {
        let mut it = Interpreter::new();
        assert_eq!(one(&mut it, "now 5"), "now=5");
        let e = it.line("bogus").unwrap_err();
        assert_eq!(e.line, 2);
        assert_eq!(e.to_string(), "diffdrive: line 2: unknown command bogus");
        for (line, want) in [
            ("now", "usage: now N"),
            ("now x", "not a number: x"),
            ("now +5", "not a number: +5"),
            ("now 5x", "not a u64: 5x"),
            (
                "now 18446744073709551616",
                "not a u64: 18446744073709551616",
            ),
            ("advance", "usage: advance N"),
            ("sync now", "usage: sync"),
            ("echo\tx", "unknown command echo"),
            ("gcra 0", "usage: gcra I OP ..."),
            ("gcra 16 state", "object index 16 out of range"),
            ("gcra 1 state", "gcra 1 is not initialised"),
            ("gcra 0 init 1 1", "usage: gcra I init T P B"),
            ("gcra 0 fly", "unknown gcra op fly"),
            ("gcra 0 take", "usage: gcra I take COST"),
            ("gcra 0 take_upto", "usage: gcra I take_upto MAX"),
            ("gcra 0 take_range 1", "usage: gcra I take_range MIN MAX"),
            ("gcra 0 refund", "usage: gcra I refund N"),
            ("gcra 0 remaining 1", "usage: gcra I remaining"),
            ("gcra 0 reconfigure 1", "usage: gcra I reconfigure T P B"),
            ("gcra 0 state 1", "usage: gcra I state"),
            ("table 0", "usage: table I OP ..."),
            ("table 1 stats", "table 1 does not exist"),
            (
                "table 0 create 4 1 1 1000 1 deny 00",
                "table 0 already exists",
            ),
            (
                "table 1 create 4 1 1 1000 1 drop 000102030405060708090a0b0c0d0e0f",
                "unknown full policy drop",
            ),
            (
                "table 1 create 4 1 1 1000 1 deny 0001",
                "hash key must be 32 hex digits",
            ),
            (
                "table 1 create 4 1 1 1000 1 deny -",
                "hash key must be 32 hex digits",
            ),
            (
                "table 1 create 4 1",
                "usage: table I create MAX SHARDS T P B deny|evict HASHKEY",
            ),
            (
                "table 1 create 4294967296 1 1 1000 1 deny 000102030405060708090a0b0c0d0e0f",
                "not a u32: 4294967296",
            ),
            ("table 0 reconfigure 1", "usage: table I reconfigure T P B"),
            ("table 0 sweep", "usage: table I sweep NSHARDS"),
            ("table 0 stats 1", "usage: table I stats"),
            ("table 0 check 1", "usage: table I check"),
            ("table 0 destroy 1", "usage: table I destroy"),
            ("table 0 take", "usage: table I take KEY ..."),
            ("table 0 take 6", "bad hex of 1 digits: 6"),
            ("table 0 take 6g 1", "bad hex: 6g"),
            ("table 0 take 61", "usage: table I take KEY COST"),
            ("table 0 take_upto 61", "usage: table I take_upto KEY MAX"),
            (
                "table 0 take_range 61 1",
                "usage: table I take_range KEY MIN MAX",
            ),
            ("table 0 refund 61", "usage: table I refund KEY N"),
            ("table 0 hold 61", "usage: table I hold KEY MAX_HOLDS"),
            ("table 0 release 61 1", "usage: table I release KEY"),
            ("table 0 remove 61 1", "usage: table I remove KEY"),
            ("table 0 get 61 1", "usage: table I get KEY"),
            ("table 0 fly 61", "unknown table op fly"),
            ("window 0", "usage: window I OP ..."),
            ("window 1 count", "window 1 is not initialised"),
            ("window 0 init 1", "usage: window I init WINDOW_NS SLOTS"),
            ("window 0 add", "usage: window I add N"),
            ("window 0 count 1", "usage: window I count"),
            ("window 0 fly", "unknown window op fly"),
            ("a b c d e f g h i j k l m", "more than 12 words"),
        ] {
            assert_eq!(bad(line), want, "{line:?}");
        }
        let long = format!("echo {}", "x".repeat(MAX_LINE));
        assert_eq!(bad(&long), "line longer than 4094 bytes");
        let key = format!("table 0 take {} 1", "61".repeat(MAX_HEX_KEY + 1));
        assert_eq!(
            bad(&key),
            format!("bad hex of 130 digits: {}", "61".repeat(65))
        );
    }

    #[test]
    fn formatting_follows_the_driver() {
        let mut it = Interpreter::new();
        assert_eq!(it.line("").unwrap(), None);
        assert_eq!(it.line("  # a comment").unwrap(), None);
        assert_eq!(it.line(" \t ").unwrap(), None);
        assert_eq!(one(&mut it, "echo"), "");
        assert_eq!(one(&mut it, "\techo  two  spaces \r"), " two  spaces ");
        assert_eq!(one(&mut it, "\tnow\t7\t"), "now=7");
        assert_eq!(
            one(&mut it, "advance 18446744073709551615"),
            "now=18446744073709551615"
        );
        assert_eq!(one(&mut it, "sync"), "sync");
        // A word that only starts with "echo" is a command like any other.
        assert_eq!(
            it.line("echoes").unwrap_err().message,
            "unknown command echoes"
        );
    }

    #[test]
    fn errors_print_libpacer_names() {
        assert_eq!(err_name(Error::InvalidRate), "EINVAL");
        assert_eq!(err_name(Error::InvalidConfig), "EINVAL");
        assert_eq!(err_name(Error::InvalidKey), "EINVAL");
        assert_eq!(err_name(Error::InvalidWindow), "EINVAL");
        assert_eq!(err_name(Error::CostExceedsBurst), "ERANGE");
        assert_eq!(err_name(Error::TableFull), "EFULL");
        assert_eq!(err_name(Error::NoSuchKey), "ENOENT");
        assert_eq!(err_name(Error::HoldLimit), "ELIMIT");
    }

    #[test]
    fn failed_inits_keep_the_object_they_replace() {
        let mut it = Interpreter::new();
        assert_eq!(one(&mut it, "gcra 0 init 0 1 1"), "rc=EINVAL");
        assert_eq!(
            one(&mut it, "gcra 0 init 1 1000 2"),
            "rc=0 tat=0 iv=64000 tol=128000"
        );
        assert_eq!(
            one(&mut it, "gcra 0 init 0 1 1"),
            "rc=EINVAL tat=0 iv=64000 tol=128000"
        );
        assert_eq!(one(&mut it, "window 0 init 3 4"), "rc=EINVAL");
        assert_eq!(one(&mut it, "window 0 init 400 4"), "rc=0 start=0 total=0");
        assert_eq!(one(&mut it, "window 0 add 3"), "ok start=0 total=3");
        assert_eq!(
            one(&mut it, "window 0 init 3 4"),
            "rc=EINVAL start=0 total=3"
        );
    }

    #[test]
    fn hex_keys_round_trip() {
        assert_eq!(hex_key(b""), "-");
        assert_eq!(hex_key(b"a\x00\xff"), "6100ff");
        assert_eq!(parse_hex("6100FF", 3).unwrap(), b"a\x00\xff");
        assert_eq!(parse_hex("-", 0).unwrap(), b"");
    }

    #[test]
    fn serve_writes_answers_then_the_error() {
        let mut out = Vec::new();
        let r = serve(&b"now 3\n# c\nsync\nnope\nnow 4\n"[..], &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "now=3\nsync\n");
        assert_eq!(r.unwrap_err().line, 4);
        let mut out = Vec::new();
        let r = serve(&b"now 3\n\xff\n"[..], &mut out).unwrap();
        assert_eq!(r.unwrap_err().to_string(), "diffdrive: line 2: not UTF-8");
        let mut out = Vec::new();
        serve(&b"now 3"[..], &mut out).unwrap().unwrap();
        assert_eq!(out, b"now=3\n");
    }

    #[test]
    fn outcomes_are_keyed_by_op_and_code() {
        for (op, answer, want) in [
            (
                "table 0 take 61 1",
                "rc=EFULL granted=0",
                "table take rc=EFULL",
            ),
            ("table 3 get 61", "absent", "table get absent"),
            ("table 3 get 61", "tat=4 holds=0", "table get present"),
            ("table 3 check", "check=0", "table check check=0"),
            ("gcra 1 take_upto 5", "got=5 granted=5", "gcra take_upto"),
            ("advance 5", "now=5", "advance"),
        ] {
            assert_eq!(outcome(op, answer), want);
        }
    }

    fn transcript_of(stdout: &str, ok: bool) -> Transcript {
        let status = std::process::Command::new(if ok { "true" } else { "false" })
            .status()
            .unwrap();
        Transcript {
            status,
            stdout: stdout.to_owned(),
            stderr: if ok { String::new() } else { "boom\n".into() },
        }
    }

    #[test]
    fn compare_reports_the_first_difference_with_context() {
        let ops: Vec<String> = (0..12).map(|i| format!("now {i}")).collect();
        let ours: Vec<String> = (0..12).map(|i| format!("now={i}")).collect();
        let mut theirs = ours.clone();
        assert_eq!(
            compare(
                9,
                12,
                &ops,
                &transcript_of(&(theirs.join("\n") + "\n"), true),
                &ours
            ),
            None
        );
        theirs[10] = "now=99".into();
        let d = compare(9, 12, &ops, &transcript_of(&theirs.join("\n"), true), &ours).unwrap();
        assert_eq!((d.seed, d.step, d.op.as_str()), (9, 11, "now 10"));
        assert_eq!(d.c.as_deref(), Some("now=99"));
        assert_eq!(d.rust.as_deref(), Some("now=10"));
        assert_eq!(d.before.len(), CONTEXT);
        assert_eq!(d.before[0], (3, "now 2".into(), "now=2".into()));
        let text = d.to_string();
        assert!(
            text.starts_with("first divergence: seed 9 step 11\n"),
            "{text}"
        );
        assert!(
            text.contains("     C    -> now=99\n     Rust -> now=10\n"),
            "{text}"
        );
        assert!(
            text.ends_with(
                "replay: edgefront-difftest pacer --seed 9 --scripts 1 --ops 12\n\
                 script: edgefront-difftest pacer-script --seed 9 --ops 12"
            ),
            "{text}"
        );
    }

    #[test]
    fn compare_reports_short_long_and_failed_transcripts() {
        let ops: Vec<String> = vec!["now 1".into(), "now 2".into()];
        let ours: Vec<String> = vec!["now=1".into(), "now=2".into()];

        let d = compare(1, 2, &ops, &transcript_of("now=1\n", false), &ours).unwrap();
        assert_eq!((d.step, d.c.as_deref()), (2, None));
        assert_eq!(d.driver_failure.as_deref(), Some("exit status: 1: boom"));
        let text = d.to_string();
        assert!(text.contains("C    -> (nothing)"), "{text}");
        assert!(
            text.contains("the C driver failed: exit status: 1: boom"),
            "{text}"
        );

        let d = compare(
            1,
            2,
            &ops,
            &transcript_of("now=1\nnow=2\nextra\n", true),
            &ours,
        )
        .unwrap();
        assert_eq!((d.step, d.c.as_deref(), d.rust), (3, Some("extra"), None));

        let d = compare(1, 2, &ops, &transcript_of("now=1\nnow=2\n", false), &ours).unwrap();
        assert_eq!(d.step, 2);
        assert!(d.note.unwrap().contains("did not exit cleanly"));

        let d = compare(1, 2, &ops, &transcript_of("now=1\n", true), &ours[..1]).unwrap();
        assert_eq!((d.step, d.c, d.rust), (2, None, None));
    }

    #[test]
    fn a_failed_invariant_check_is_reported_even_when_both_agree() {
        let ops = vec!["table 0 check".to_owned()];
        let ours = vec!["check=3".to_owned()];
        let d = compare(1, 1, &ops, &transcript_of("check=3\n", true), &ours).unwrap();
        assert_eq!(d.note.as_deref(), Some("the table invariants do not hold"));
        assert!(
            d.to_string()
                .contains("  the table invariants do not hold\n")
        );
    }
}
