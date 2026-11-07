//! pacerd's config reader against edgefront-limiter's, on every config in
//! legacy/pacer.
//!
//! The C side is `diffdrive config [PATH]`, which runs `pd_config_load` and
//! prints `error MESSAGE`, or `ok` and every parsed field in a fixed layout
//! (see the top of `legacy/pacer/tests/diffdrive.c`). [`rust_answer`] prints
//! the same layout from [`edgefront_limiter::config`], so the two answers
//! are compared as text: accept or reject, the error message with its line
//! number, and, for an accepted config, every value it set.
//!
//! The inputs ([`corpus`]) are:
//!
//! - every file under `tests`, `examples` and `fuzz/corpus`, read by each
//!   side's own file reader (`pd_config_load` and `read_pacerd`), whether
//!   or not it is meant to be a config;
//! - every string literal in `tests/*.c` and `fuzz/*.c`, which is where
//!   `test_config.c` and the integration tests keep their configs, with
//!   adjacent literals joined as the C compiler joins them. A literal with
//!   printf conversions is also tried with them filled in (numbers become
//!   `9000`, strings become empty), as the integration tests fill them;
//! - each literal again inside the one-listener block that
//!   `expect_listener_error` in `test_config.c` wraps around a directive.
//!
//! On top of these, [`Plan::mutants`] seeded mutants of the corpus (byte
//! flips, tokens from `fuzz/config.dict`, dropped, repeated, swapped and
//! spliced lines, generated directives and truncation) go to the C side on
//! stdin and to [`parse_pacerd`] directly. Mutant `i` comes from seed
//! `seed + i` alone, so `edgefront-difftest pacer-config-mutant --seed S`
//! prints any one of them again.

use std::fmt;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use edgefront_limiter::config::{LimiterConfig, ListenerConfig, read_pacerd};
use edgefront_limiter::{FullPolicy, Rate, parse_pacerd};

use crate::driver;
use crate::pacer::DriverCommand;
use crate::rng::Rng;

/// The directories of legacy/pacer whose files are all tried as configs.
pub const CORPUS_DIRS: [&str; 3] = ["tests", "examples", "fuzz/corpus"];

/// The block `expect_listener_error` in `tests/test_config.c` puts around a
/// directive: everything before it and everything after it.
pub const LISTENER_BLOCK: (&str, &str) = (
    "listener a {\n listen 127.0.0.1:1\n upstream 127.0.0.1:2\n ",
    "\n}\n",
);

/// How many mutants each corpus input yields at most from one seed.
const MUTATIONS_MAX: usize = 4;
/// Answers longer than this are cut in reports.
const SHOWN_ANSWER: usize = 600;

/// The path of legacy/pacer in this repository.
pub fn pacer_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../legacy/pacer")
}

/// Where an input comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A file, which each side reads with its own file reader.
    File(PathBuf),
    /// Bytes, sent to the C driver on stdin and to [`parse_pacerd`].
    Bytes(Vec<u8>),
}

/// One config to put through both parsers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    /// What the report calls it: a path, a literal's file and line, or a
    /// mutant's seed.
    pub name: String,
    pub source: Source,
}

impl Input {
    /// The input's bytes: the file's contents or the bytes themselves.
    pub fn bytes(&self) -> io::Result<Vec<u8>> {
        match &self.source {
            Source::File(path) => fs::read(path),
            Source::Bytes(b) => Ok(b.clone()),
        }
    }
}

/// The answer the C driver gives for a parse result, from the Rust parser.
pub fn answer(result: &Result<LimiterConfig, String>) -> String {
    let cfg = match result {
        Ok(cfg) => cfg,
        Err(e) => return format!("error {e}\n"),
    };
    let admin = match &cfg.admin_socket {
        Some(path) => hex(path.as_os_str().as_encoded_bytes()),
        None => "-".to_owned(),
    };
    let mut out = format!(
        "ok\nworkers={} admin_socket={admin} drain_timeout_ns={} log_level={} listeners={}\n",
        cfg.workers,
        cfg.drain_timeout.as_nanos(),
        cfg.log_level.as_str(),
        cfg.listeners.len()
    );
    for l in &cfg.listeners {
        out.push_str(&listener_line(l));
    }
    out
}

fn listener_line(l: &ListenerConfig) -> String {
    // pacerd stores `min_rate off` as zero bytes; the window is then unused.
    let min_rate = match l.min_rate {
        Some(m) if m.bytes != 0 => format!("{}/{}", m.bytes, m.window.as_nanos()),
        _ => "off".to_owned(),
    };
    let full = match l.table_full {
        FullPolicy::Deny => "deny",
        FullPolicy::Evict => "evict",
    };
    format!(
        "listener {} listen={} upstream={} max_conns={} buffer={} connect_timeout_ns={} \
         idle_timeout_ns={} min_rate={min_rate} per_ip_max_conns={} ipv6_prefix={} \
         per_ip_conn_rate={} per_ip_bandwidth={} bandwidth={} table_size={} table_full={full}\n",
        l.name,
        addr(&l.listen),
        addr(&l.upstream),
        l.max_conns,
        l.buffer_size,
        l.connect_timeout.as_nanos(),
        l.idle_timeout.as_nanos(),
        l.per_ip_max_conns,
        l.ipv6_prefix,
        rate(l.per_ip_conn_rate),
        rate(l.per_ip_bandwidth),
        rate(l.bandwidth),
        l.table_size,
    )
}

fn addr(a: &SocketAddr) -> String {
    match a {
        SocketAddr::V4(v4) => format!("4:{}:{}", hex(&v4.ip().octets()), v4.port()),
        SocketAddr::V6(v6) => format!(
            "6:{}:{}:{}:{}",
            hex(&v6.ip().octets()),
            v6.port(),
            v6.flowinfo(),
            v6.scope_id()
        ),
    }
}

fn rate(r: Option<Rate>) -> String {
    match r {
        Some(r) => format!("{}/{}/{}", r.tokens, r.period_ns, r.burst),
        None => "off".to_owned(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The Rust parser's answer for an input.
pub fn rust_answer(input: &Input) -> String {
    match &input.source {
        Source::File(path) => answer(&read_pacerd(path)),
        Source::Bytes(b) => answer(&parse_pacerd(b).map_err(|e| e.to_string())),
    }
}

/// The C parser's answer for an input, from `DRIVER ARGS... config [PATH]`.
/// A driver that fails, writes to stderr or prints something that is not
/// an answer is an error, not an answer.
pub fn c_answer(driver: &DriverCommand, input: &Input) -> io::Result<String> {
    let mut args = driver.args.clone();
    args.push("config".to_owned());
    let stdin: &[u8] = match &input.source {
        Source::File(path) => {
            args.push(path.to_string_lossy().into_owned());
            &[]
        }
        Source::Bytes(b) => b,
    };
    let t = driver::run_with_input(&driver.program, &args, stdin)?;
    if !t.success() || !t.stderr.is_empty() {
        return Err(io::Error::other(format!(
            "{} exited with {} on {}: {}",
            driver.program.display(),
            t.status,
            input.name,
            t.stderr.trim_end()
        )));
    }
    if !(t.stdout.starts_with("ok\n") || t.stdout.starts_with("error ")) {
        return Err(io::Error::other(format!(
            "{} printed no answer for {}: {:?}",
            driver.program.display(),
            input.name,
            t.stdout
        )));
    }
    Ok(t.stdout)
}

/// The `line N` of an error answer, or `None` for an accepted config or an
/// error that names no line (a file that could not be read).
pub fn error_line(answer: &str) -> Option<usize> {
    let rest = answer.strip_prefix("error line ")?;
    let digits = rest.split(':').next()?;
    digits.parse().ok()
}

// ---------------------------------------------------------------------------
// The corpus.

/// Every input in legacy/pacer at `root`, in a fixed order: the files, then
/// the literals, the literals with their conversions filled in, and the
/// literals inside a listener block.
pub fn corpus(root: &Path) -> io::Result<Vec<Input>> {
    let mut inputs = Vec::new();
    for dir in CORPUS_DIRS {
        let mut files = Vec::new();
        walk(&root.join(dir), &mut files)?;
        files.sort();
        for path in files {
            let name = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            inputs.push(Input {
                name,
                source: Source::File(path),
            });
        }
    }
    let mut sources = Vec::new();
    for dir in ["tests", "fuzz"] {
        for entry in fs::read_dir(root.join(dir))? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "c") {
                sources.push(path);
            }
        }
    }
    sources.sort();
    let mut literals = Vec::new();
    for path in &sources {
        let text = fs::read(path)?;
        let file = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        for (line, bytes) in c_string_literals(&text) {
            literals.push((format!("{file}:{line}"), bytes));
        }
    }
    for (name, bytes) in &literals {
        inputs.push(Input {
            name: format!("literal {name}"),
            source: Source::Bytes(bytes.clone()),
        });
    }
    for (name, bytes) in &literals {
        if let Some(filled) = fill_conversions(bytes) {
            inputs.push(Input {
                name: format!("literal {name}, filled in"),
                source: Source::Bytes(filled),
            });
        }
    }
    for (name, bytes) in &literals {
        let mut block = LISTENER_BLOCK.0.as_bytes().to_vec();
        block.extend_from_slice(bytes);
        block.extend_from_slice(LISTENER_BLOCK.1.as_bytes());
        inputs.push(Input {
            name: format!("literal {name}, in a listener block"),
            source: Source::Bytes(block),
        });
    }
    Ok(inputs)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            walk(&entry.path(), out)?;
        } else if kind.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}

/// The string literals of a C source, each with the line it starts on.
/// Literals separated only by whitespace are joined, as in C. Comments,
/// character constants and `#include <...>` are skipped. Escapes are
/// decoded: the simple ones, octal (up to three digits) and hex.
pub fn c_string_literals(src: &[u8]) -> Vec<(usize, Vec<u8>)> {
    let mut out: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut line = 1;
    let mut i = 0;
    // Whether the last token was a literal, so the next one joins it.
    let mut joining = false;
    while i < src.len() {
        match src[i] {
            b'\n' => {
                line += 1;
                i += 1;
            }
            b' ' | b'\t' | b'\r' | b'\x0c' | b'\x0b' => i += 1,
            b'/' if src.get(i + 1) == Some(&b'/') => {
                while i < src.len() && src[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if src.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < src.len() && !(src[i] == b'*' && src.get(i + 1) == Some(&b'/')) {
                    line += usize::from(src[i] == b'\n');
                    i += 1;
                }
                i += 2;
            }
            quote @ (b'"' | b'\'') => {
                let start = line;
                let mut bytes = Vec::new();
                i += 1;
                while i < src.len() && src[i] != quote && src[i] != b'\n' {
                    if src[i] == b'\\' {
                        i = unescape(src, i + 1, &mut bytes);
                    } else {
                        bytes.push(src[i]);
                        i += 1;
                    }
                }
                // The closing quote; a newline ends an unterminated one and
                // is left for the line count.
                if src.get(i) == Some(&quote) {
                    i += 1;
                }
                if quote == b'\'' {
                    joining = false;
                } else if joining {
                    if let Some(last) = out.last_mut() {
                        last.1.extend_from_slice(&bytes);
                    }
                } else {
                    out.push((start, bytes));
                    joining = true;
                }
            }
            _ => {
                joining = false;
                i += 1;
            }
        }
    }
    out
}

/// Decodes the escape after a backslash at `src[i - 1]` and returns the
/// index after it.
fn unescape(src: &[u8], mut i: usize, out: &mut Vec<u8>) -> usize {
    let Some(&c) = src.get(i) else {
        return i;
    };
    let simple = match c {
        b'n' => Some(b'\n'),
        b't' => Some(b'\t'),
        b'r' => Some(b'\r'),
        b'a' => Some(0x07),
        b'b' => Some(0x08),
        b'f' => Some(0x0c),
        b'v' => Some(0x0b),
        b'\\' | b'"' | b'\'' | b'?' => Some(c),
        _ => None,
    };
    if let Some(b) = simple {
        out.push(b);
        return i + 1;
    }
    if c == b'x' {
        i += 1;
        let mut v: u8 = 0;
        while let Some(d) = src.get(i).and_then(|&d| char::from(d).to_digit(16)) {
            v = v.wrapping_mul(16).wrapping_add(d as u8);
            i += 1;
        }
        out.push(v);
        return i;
    }
    if c.is_ascii_digit() && c < b'8' {
        let mut v: u8 = 0;
        let end = i + 3;
        while i < end && src.get(i).is_some_and(|d| (b'0'..b'8').contains(d)) {
            v = v.wrapping_mul(8).wrapping_add(src[i] - b'0');
            i += 1;
        }
        out.push(v);
        return i;
    }
    // An unknown escape keeps its character, as compilers do with a warning.
    out.push(c);
    i + 1
}

/// The literal with its printf conversions filled in: integer conversions
/// become `9000` and `%s` becomes empty. `None` if it has none.
pub fn fill_conversions(s: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut filled = false;
    let mut i = 0;
    while i < s.len() {
        if s[i] != b'%' {
            out.push(s[i]);
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < s.len() && b"-+ #0123456789.*".contains(&s[j]) {
            j += 1;
        }
        while j < s.len() && b"hlzjt".contains(&s[j]) {
            j += 1;
        }
        match s.get(j) {
            Some(b'%') if j == i + 1 => out.push(b'%'),
            Some(b'd' | b'i' | b'u' | b'x' | b'X') => {
                out.extend_from_slice(b"9000");
                filled = true;
            }
            Some(b's') => filled = true,
            _ => {
                out.extend_from_slice(&s[i..(j + 1).min(s.len())]);
            }
        }
        i = j + 1;
    }
    filled.then_some(out)
}

/// The tokens of a libFuzzer dictionary: one quoted string per line, with
/// `\\`, `\"` and `\xNN` escapes; comments and blank lines are skipped.
pub fn parse_dict(text: &[u8]) -> Vec<Vec<u8>> {
    let mut tokens = Vec::new();
    for line in text.split(|&c| c == b'\n') {
        let Some(start) = line.iter().position(|&c| c == b'"') else {
            continue;
        };
        if line.iter().take(start).any(|&c| c == b'#') {
            continue;
        }
        let Some(end) = line.iter().rposition(|&c| c == b'"').filter(|&e| e > start) else {
            continue;
        };
        let body = &line[start + 1..end];
        let mut token = Vec::new();
        let mut i = 0;
        while i < body.len() {
            if body[i] == b'\\' && body.get(i + 1) == Some(&b'x') {
                let hi = body.get(i + 2).and_then(|&d| char::from(d).to_digit(16));
                let lo = body.get(i + 3).and_then(|&d| char::from(d).to_digit(16));
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    token.push((hi * 16 + lo) as u8);
                    i += 4;
                    continue;
                }
            }
            if body[i] == b'\\' && i + 1 < body.len() {
                token.push(body[i + 1]);
                i += 2;
                continue;
            }
            token.push(body[i]);
            i += 1;
        }
        tokens.push(token);
    }
    tokens
}

// ---------------------------------------------------------------------------
// Mutants.

/// What mutants are made from: the corpus's bytes and the dictionary.
#[derive(Debug, Clone)]
pub struct MutationBase {
    /// Corpus inputs that edgefront-limiter accepts: the configs.
    pub configs: Vec<Vec<u8>>,
    /// Every corpus input.
    pub all: Vec<Vec<u8>>,
    /// `fuzz/config.dict`.
    pub dict: Vec<Vec<u8>>,
}

impl MutationBase {
    /// The base for the corpus of legacy/pacer at `root`.
    pub fn load(root: &Path, corpus: &[Input]) -> io::Result<Self> {
        let mut all = Vec::with_capacity(corpus.len());
        for input in corpus {
            all.push(input.bytes()?);
        }
        let configs = all
            .iter()
            .filter(|b| parse_pacerd(b).is_ok())
            .cloned()
            .collect();
        let dict = parse_dict(&fs::read(root.join("fuzz/config.dict"))?);
        Ok(Self { configs, all, dict })
    }

    /// The mutant for `seed`: a corpus input, three times in four a config,
    /// with one mutation half the time and up to four otherwise, so that
    /// many mutants still parse and their values get compared.
    pub fn mutant(&self, seed: u64) -> Vec<u8> {
        let mut rng = Rng::new(seed);
        let pool = if self.configs.is_empty() || rng.chance(0.25) {
            &self.all
        } else {
            &self.configs
        };
        let mut bytes = if pool.is_empty() {
            Vec::new()
        } else {
            pool[rng.below(pool.len())].clone()
        };
        let n = if rng.chance(0.5) {
            1
        } else {
            rng.range(2, MUTATIONS_MAX)
        };
        for _ in 0..n {
            self.mutate(&mut rng, &mut bytes);
        }
        bytes
    }

    fn mutate(&self, rng: &mut Rng, bytes: &mut Vec<u8>) {
        let mut lines: Vec<Vec<u8>> = bytes.split(|&c| c == b'\n').map(<[u8]>::to_vec).collect();
        let at = rng.below(lines.len());
        match rng.below(9) {
            // Change one byte, to anything or to a byte that matters here.
            0 if !bytes.is_empty() => {
                let i = rng.below(bytes.len());
                bytes[i] = if rng.chance(0.5) {
                    rng.next_u64() as u8
                } else {
                    rng.pick(b" \t\r\n#{}/:[]0123456789kmgsh\0\"")
                };
                return;
            }
            // Put a dictionary token into a line.
            1 if !self.dict.is_empty() => {
                let token = &self.dict[rng.below(self.dict.len())];
                let line = &mut lines[at];
                let i = rng.range(0, line.len());
                let mut spliced = line[..i].to_vec();
                if rng.chance(0.5) {
                    spliced.push(b' ');
                }
                spliced.extend_from_slice(token);
                spliced.extend_from_slice(&line[i..]);
                *line = spliced;
            }
            2 if lines.len() > 1 => {
                lines.remove(at);
            }
            3 => {
                let copy = lines[at].clone();
                lines.insert(at, copy);
            }
            4 => {
                let other = rng.below(lines.len());
                lines.swap(at, other);
            }
            // A line from another input.
            5 if !self.all.is_empty() => {
                let donor = &self.all[rng.below(self.all.len())];
                let donor_lines: Vec<&[u8]> = donor.split(|&c| c == b'\n').collect();
                let line = donor_lines[rng.below(donor_lines.len())].to_vec();
                lines.insert(at, line);
            }
            6 => {
                let line = self.directive(rng);
                lines.insert(at, line);
            }
            7 if !bytes.is_empty() => {
                bytes.truncate(rng.below(bytes.len()));
                return;
            }
            // Replace a directive's value with a generated one.
            _ => {
                let words: Vec<&[u8]> = lines[at]
                    .split(|&c| c == b' ' || c == b'\t')
                    .filter(|w| !w.is_empty())
                    .collect();
                if let Some(first) = words.first() {
                    let mut line = first.to_vec();
                    line.push(b' ');
                    line.extend_from_slice(&value(rng));
                    lines[at] = line;
                }
            }
        }
        *bytes = lines.join(&b'\n');
    }

    /// A directive line: half the time a real directive with a value at or
    /// near one of its limits ([`DIRECTIVE_VALUES`]), otherwise words from
    /// the dictionary and [`value`].
    fn directive(&self, rng: &mut Rng) -> Vec<u8> {
        let mut line: Vec<u8> = if rng.chance(0.15) {
            Vec::new()
        } else {
            b"    ".to_vec()
        };
        if rng.chance(0.5) {
            let (name, values) = DIRECTIVE_VALUES[rng.below(DIRECTIVE_VALUES.len())];
            line.extend_from_slice(name.as_bytes());
            line.push(b' ');
            line.extend_from_slice(rng.pick(values).as_bytes());
            return line;
        }
        if self.dict.is_empty() {
            line.extend_from_slice(b"workers");
        } else {
            line.extend_from_slice(&self.dict[rng.below(self.dict.len())]);
        }
        for _ in 0..rng.range(0, 3) {
            let word = if rng.chance(0.3) && !self.dict.is_empty() {
                self.dict[rng.below(self.dict.len())].clone()
            } else {
                value(rng)
            };
            line.extend_from_slice(b" ");
            line.extend_from_slice(&word);
        }
        line
    }
}

/// Each directive with values on both sides of its limits.
const DIRECTIVE_VALUES: [(&str, &[&str]); 18] = [
    ("workers", &["1", "64", "0", "65", "8"]),
    (
        "admin_socket",
        &["/run/p.sock", "relative.sock", "/", "/a/b c"],
    ),
    (
        "drain_timeout",
        &["0", "0s", "1h", "3600s", "61m", "250ms", "1d"],
    ),
    (
        "log_level",
        &["debug", "info", "warn", "error", "INFO", "trace"],
    ),
    (
        "listen",
        &[
            "0.0.0.0:8080",
            "[::]:2222",
            "[::ffff:10.0.0.1]:1",
            "10.0.0.1",
        ],
    ),
    (
        "upstream",
        &["127.0.0.1:9000", "[2001:db8::1]:443", "[::1]", "a.b:80"],
    ),
    ("max_conns", &["1", "1000000", "0", "1000001", "1k", "5000"]),
    (
        "buffer",
        &["1k", "1024", "1023", "1m", "1025k", "16k", "2g"],
    ),
    (
        "connect_timeout",
        &["1ms", "999us", "600s", "10m", "601s", "5s"],
    ),
    (
        "idle_timeout",
        &["1s", "999ms", "24h", "86400s", "86401s", "2m"],
    ),
    (
        "min_rate",
        &[
            "off",
            "64 per 10s",
            "1 per 1s",
            "1 per 10m",
            "0 per 1s",
            "1 per 999ms",
            "1 per 601s",
            "1k per 1m",
            "64 per",
        ],
    ),
    ("per_ip_max_conns", &["0", "20", "1000000", "1000001"]),
    ("ipv6_prefix", &["32", "128", "31", "129", "56", "64"]),
    (
        "per_ip_conn_rate",
        &[
            "10/s",
            "10/s burst 20",
            "600/m",
            "1/h",
            "0/s",
            "1m/s burst 1g",
            "10/s burst 0",
            "10/d",
            "10/s bursts 2",
            "1125899906842624/s burst 1125899906842624",
        ],
    ),
    (
        "per_ip_bandwidth",
        &["1m/s burst 256k", "1g/h", "16k/m burst 1", "1/s burst"],
    ),
    ("bandwidth", &["100m/s burst 1m", "600/m", "1/s", "1k/ms"]),
    (
        "table_size",
        &["16", "15", "16777216", "16777217", "4096", "16m"],
    ),
    ("table_full", &["deny", "evict", "drop", "Deny"]),
];

/// A value for a directive: a number near a limit, with or without a unit,
/// a rate, an address or a path.
fn value(rng: &mut Rng) -> Vec<u8> {
    const NUMBERS: [&str; 22] = [
        "0",
        "1",
        "15",
        "16",
        "32",
        "64",
        "128",
        "129",
        "1023",
        "1024",
        "1000000",
        "1000001",
        "16777216",
        "16777217",
        "4294967295",
        "4294967296",
        "1125899906842624",
        "1125899906842625",
        "18446744073709551615",
        "18446744073709551616",
        "00000000000000000000001",
        "000000000000000000000001",
    ];
    const UNITS: [&str; 12] = ["", "k", "m", "g", "K", "ms", "s", "m", "h", "us", "d", "x"];
    const ADDRS: [&str; 12] = [
        "127.0.0.1:80",
        "0.0.0.0:65535",
        "1.2.3.4:0",
        "1.2.3.4:65536",
        "01.2.3.4:80",
        "1.2.3:80",
        "[::1]:443",
        "[::ffff:1.2.3.4]:80",
        "[1:2:3:4:5:6:7:8]:80",
        "[::1%1]:80",
        "[fe80::1]:080",
        "::1:80",
    ];
    let pick_number = |rng: &mut Rng| -> String {
        if rng.chance(0.3) {
            (rng.next_u64() >> rng.below(64)).to_string()
        } else {
            rng.pick(&NUMBERS).to_owned()
        }
    };
    let text = match rng.below(6) {
        0 | 1 => format!("{}{}", pick_number(rng), rng.pick(&UNITS)),
        2 => format!(
            "{}/{}",
            pick_number(rng),
            rng.pick(&["s", "m", "h", "d", "", "ms"])
        ),
        3 => rng.pick(&ADDRS).to_owned(),
        4 => {
            let len = rng.range(0, 120);
            let mut p = String::from("/");
            p.extend(std::iter::repeat_n('a', len));
            p
        }
        _ => rng
            .pick(&[
                "off", "deny", "evict", "debug", "error", "{", "}", "per", "burst",
            ])
            .to_owned(),
    };
    text.into_bytes()
}

// ---------------------------------------------------------------------------
// Runs and reports.

/// How many mutants to add to the corpus, and from which seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub mutants: usize,
    pub seed: u64,
}

impl Plan {
    /// The seed of mutant `i`.
    pub fn mutant_seed(&self, i: usize) -> u64 {
        self.seed.wrapping_add(i as u64)
    }
}

/// An input the two parsers answered differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    pub name: String,
    /// The mutant's seed, when the input is a mutant.
    pub seed: Option<u64>,
    pub c: String,
    pub rust: String,
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "divergence on {}", self.name)?;
        let c: Vec<&str> = self.c.lines().collect();
        let rust: Vec<&str> = self.rust.lines().collect();
        let first = (0..c.len().max(rust.len()))
            .find(|&i| c.get(i) != rust.get(i))
            .unwrap_or(0);
        writeln!(f, "  first differing answer line: {}", first + 1)?;
        writeln!(
            f,
            "  C:    {}",
            cut(c.get(first).copied().unwrap_or("(nothing)"))
        )?;
        writeln!(
            f,
            "  Rust: {}",
            cut(rust.get(first).copied().unwrap_or("(nothing)"))
        )?;
        writeln!(
            f,
            "  error line: C {}, Rust {}",
            line_text(error_line(&self.c)),
            line_text(error_line(&self.rust))
        )?;
        if let Some(seed) = self.seed {
            writeln!(
                f,
                "  replay: edgefront-difftest pacer-config-mutant --seed {seed}"
            )?;
        }
        Ok(())
    }
}

fn line_text(line: Option<usize>) -> String {
    line.map_or_else(|| "-".to_owned(), |l| l.to_string())
}

fn cut(s: &str) -> String {
    if s.len() <= SHOWN_ANSWER {
        return s.to_owned();
    }
    let mut end = SHOWN_ANSWER;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// The result of a run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub seed: u64,
    /// Corpus files, string literals (with their variants) and mutants run.
    pub files: usize,
    pub literals: usize,
    pub mutants: usize,
    /// Inputs both parsers accepted.
    pub accepted: usize,
    /// Inputs both parsers rejected with the same message and line.
    pub rejected: usize,
    pub divergences: Vec<Divergence>,
}

impl Report {
    /// Every input the run compared.
    pub fn inputs(&self) -> usize {
        self.files + self.literals + self.mutants
    }

    fn record(&mut self, input: &Input, seed: Option<u64>, c: String, rust: String) {
        if c != rust {
            self.divergences.push(Divergence {
                name: input.name.clone(),
                seed,
                c,
                rust,
            });
        } else if c.starts_with("ok\n") {
            self.accepted += 1;
        } else {
            self.rejected += 1;
        }
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "seed {}", self.seed)?;
        writeln!(
            f,
            "inputs: {} ({} files, {} literals, {} mutants)",
            self.inputs(),
            self.files,
            self.literals,
            self.mutants
        )?;
        writeln!(
            f,
            "agreed: {} accepted, {} rejected with the same error line",
            self.accepted, self.rejected
        )?;
        write!(f, "divergences: {}", self.divergences.len())?;
        for d in &self.divergences {
            write!(f, "\n{d}")?;
        }
        Ok(())
    }
}

/// Runs the corpus of legacy/pacer at `root` and `plan.mutants` mutants
/// through `driver` and the Rust parser and reports every divergence.
pub fn run(driver: &DriverCommand, root: &Path, plan: Plan) -> io::Result<Report> {
    let corpus = corpus(root)?;
    let mut report = Report {
        seed: plan.seed,
        ..Report::default()
    };
    for input in &corpus {
        match input.source {
            Source::File(_) => report.files += 1,
            Source::Bytes(_) => report.literals += 1,
        }
        let c = c_answer(driver, input)?;
        report.record(input, None, c, rust_answer(input));
    }
    let base = MutationBase::load(root, &corpus)?;
    for i in 0..plan.mutants {
        let seed = plan.mutant_seed(i);
        let input = Input {
            name: format!("mutant {seed}"),
            source: Source::Bytes(base.mutant(seed)),
        };
        report.mutants += 1;
        let c = c_answer(driver, &input)?;
        report.record(&input, Some(seed), c, rust_answer(&input));
    }
    Ok(report)
}

/// The mutant `seed` gives for the corpus of legacy/pacer at `root`.
pub fn mutant(root: &Path, seed: u64) -> io::Result<Vec<u8>> {
    let corpus = corpus(root)?;
    Ok(MutationBase::load(root, &corpus)?.mutant(seed))
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV6};
    use std::time::Duration;

    use edgefront_limiter::config::MinRate;

    use super::*;

    #[test]
    fn literals_are_joined_and_unescaped() {
        let src = br#"
// "not this"
/* "nor
   this" */
char c = '"';
const char *a = "one\n"
                "two\t\"q\"\\";
f("x", "y");
g("\x41\101\0z\q");
#include <stdio.h>
"#;
        let got = c_string_literals(src);
        assert_eq!(
            got,
            vec![
                (6, b"one\ntwo\t\"q\"\\".to_vec()),
                (8, b"x".to_vec()),
                (8, b"y".to_vec()),
                (9, b"AA\0zq".to_vec()),
            ]
        );
    }

    #[test]
    fn unterminated_literals_and_escapes_end_cleanly() {
        assert_eq!(c_string_literals(b"\"abc"), vec![(1, b"abc".to_vec())]);
        assert_eq!(c_string_literals(b"\"a\\"), vec![(1, b"a".to_vec())]);
        assert_eq!(c_string_literals(b"/* open"), vec![]);
        assert_eq!(
            c_string_literals(b"\"a\nb\""),
            vec![(1, b"a".to_vec()), (2, b"".to_vec())]
        );
    }

    #[test]
    fn conversions_are_filled_in() {
        assert_eq!(
            fill_conversions(b"upstream 127.0.0.1:%u\n%s%% %-5llu %zu %q").unwrap(),
            b"upstream 127.0.0.1:9000\n% 9000 9000 %q".to_vec()
        );
        assert_eq!(fill_conversions(b"100%% plain"), None);
        assert_eq!(fill_conversions(b"trailing %"), None);
    }

    #[test]
    fn dictionaries_are_decoded() {
        let dict = parse_dict(
            b"# comment\n\"workers\"\n\n\"\\x0a\"\nname=\"a\\\"b\\\\\"\n\"\\xzz\"\nbare\n",
        );
        assert_eq!(
            dict,
            vec![
                b"workers".to_vec(),
                b"\n".to_vec(),
                b"a\"b\\".to_vec(),
                b"xzz".to_vec()
            ]
        );
    }

    #[test]
    fn answers_cover_every_field() {
        let mut cfg = LimiterConfig {
            workers: 3,
            admin_socket: Some(PathBuf::from("/a b")),
            drain_timeout: Duration::from_millis(1500),
            ..LimiterConfig::default()
        };
        let mut l = ListenerConfig::new(
            "web",
            SocketAddr::from((Ipv4Addr::new(10, 0, 0, 1), 80)),
            SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 9000, 7, 2)),
        );
        l.min_rate = Some(MinRate {
            bytes: 512,
            window: Duration::from_secs(10),
        });
        l.bandwidth = Some(Rate::new(10, 1_000_000_000, 20));
        l.table_full = FullPolicy::Evict;
        cfg.listeners.push(l.clone());
        l.name = "off".into();
        l.min_rate = Some(MinRate {
            bytes: 0,
            window: Duration::from_secs(10),
        });
        cfg.listeners.push(l);
        let text = answer(&Ok(cfg));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "ok");
        assert_eq!(
            lines[1],
            "workers=3 admin_socket=2f612062 drain_timeout_ns=1500000000 log_level=info listeners=2"
        );
        assert_eq!(
            lines[2],
            "listener web listen=4:0a000001:80 \
             upstream=6:00000000000000000000000000000001:9000:7:2 max_conns=1024 buffer=16384 \
             connect_timeout_ns=5000000000 idle_timeout_ns=300000000000 \
             min_rate=512/10000000000 per_ip_max_conns=0 ipv6_prefix=64 per_ip_conn_rate=off \
             per_ip_bandwidth=off bandwidth=10/1000000000/20 table_size=16384 table_full=evict"
        );
        assert!(lines[3].contains(" min_rate=off "), "{}", lines[3]);
        assert_eq!(answer(&Err("line 2: x".into())), "error line 2: x\n");
    }

    #[test]
    fn error_lines_are_read_from_answers() {
        assert_eq!(error_line("error line 12: workers: x\n"), Some(12));
        assert_eq!(
            error_line("error line 0: config larger than 1048576 bytes\n"),
            Some(0)
        );
        assert_eq!(error_line("error /x: No such file or directory\n"), None);
        assert_eq!(error_line("ok\nworkers=1\n"), None);
    }

    #[test]
    fn mutants_depend_only_on_their_seed() {
        let base = MutationBase {
            configs: vec![
                b"listener a {\n listen 127.0.0.1:1\n upstream 127.0.0.1:2\n}\n".to_vec(),
            ],
            all: vec![b"x".to_vec(), b"listener a {\n}\n".to_vec()],
            dict: vec![b"workers".to_vec(), b"burst".to_vec()],
        };
        let a: Vec<Vec<u8>> = (0..200).map(|s| base.mutant(s)).collect();
        let b: Vec<Vec<u8>> = (0..200).rev().map(|s| base.mutant(s)).collect();
        assert!(a.iter().eq(b.iter().rev()));
        let distinct: std::collections::HashSet<_> = a.iter().collect();
        assert!(distinct.len() > 150, "{} distinct mutants", distinct.len());
    }

    #[test]
    fn an_empty_base_still_mutates() {
        let base = MutationBase {
            configs: Vec::new(),
            all: Vec::new(),
            dict: Vec::new(),
        };
        for seed in 0..200 {
            let _ = base.mutant(seed);
        }
    }

    #[test]
    fn divergences_name_the_first_differing_line() {
        let d = Divergence {
            name: "mutant 9".into(),
            seed: Some(9),
            c: "error line 3: a\n".into(),
            rust: "error line 4: a\n".into(),
        };
        let text = d.to_string();
        assert!(text.contains("first differing answer line: 1\n"), "{text}");
        assert!(text.contains("error line: C 3, Rust 4\n"), "{text}");
        assert!(
            text.contains("replay: edgefront-difftest pacer-config-mutant --seed 9"),
            "{text}"
        );
        let d = Divergence {
            name: "f".into(),
            seed: None,
            c: format!("ok\n{}\n", "w".repeat(700)),
            rust: "ok\n".into(),
        };
        let text = d.to_string();
        assert!(text.contains("first differing answer line: 2\n"), "{text}");
        assert!(text.contains("...\n  Rust: (nothing)\n"), "{text}");
        assert!(text.contains("error line: C -, Rust -\n"), "{text}");
        assert!(!text.contains("replay"), "{text}");
        assert_eq!(cut(&"é".repeat(400)).len(), 600 + 3);
    }
}
