//! `edgefront check`: lints a limiter config.
//!
//! ```text
//! edgefront check [--allow CLASS]... <file>
//! edgefront check --print <file>
//! ```
//!
//! A file ending in `.toml` is read as the limiter section of an edgefront
//! config (the serde form of `edgefront_limiter::LimiterConfig`); anything
//! else is read as a pacerd.conf. A config that does not parse is reported
//! as `--check-config` reports one, with exit code 2. A config that parses
//! is linted by `edgefront_limiter::lint`, one finding per line:
//!
//! ```text
//! pacerd.conf:14: warning[table-below-max-conns]: listener web: table_size 64 is below max_conns 100: ...
//! pacerd.conf: 1 warning
//! ```
//!
//! then exits 1, or prints `FILE: ok` and exits 0 when nothing is found.
//! A TOML config with an `[address_policy]` section also has the section
//! built, list files and all, and reports its size before the verdict:
//!
//! ```text
//! address_policy: 2 trusted proxies, 3 classes, 1000002 prefixes, 92517736 bytes
//!   class blocked: deny, 1000000 prefixes
//!   class office: allow, 1 prefixes
//!   class partners: limit, 1 prefixes
//!   list proxies.txt: 1 prefixes
//!   list blocked.txt: 1000000 prefixes
//! ```
//!
//! A section that does not build is a config that does not parse (exit 2).
//!
//! A TOML config's `[[http_limits]]` rules are built and linted too, after
//! a line with their count (`http_limits: 2 rules`); their findings name
//! the rule's line:
//!
//! ```text
//! limits.toml:7: warning[http-key-never-matches]: http limit login: key path_prefix:/admin lies outside ...
//! ```
//!
//! A TOML file with HTTP rules and no `listeners` is an HTTP-only config,
//! as `edgefront proxy --limits` reads it, and is checked as that alone.
//! `--allow CLASS` drops the findings of one class, and may be repeated.
//!
//! `--print` writes the config instead, as the TOML limiter section that
//! reads back to the same config, and exits 0. That turns a pacerd.conf
//! into its edgefront form, and shows a TOML file in canonical form:
//! values equal to pacerd's defaults left out, rates in lowest terms and
//! quotas in their whole windows.

use std::process::ExitCode;

use edgefront_limiter::LimiterConfig;
use edgefront_limiter::config::{ListenerLines, SourceMap, read_pacerd_mapped};
use edgefront_limiter::lint::{Class, lint};
use toml::de::{DeTable, DeValue};

const USAGE: &str =
    "usage: edgefront check [--allow CLASS]... <file>\n       edgefront check --print <file>\n";

/// The 1-based line holding byte `offset` of `text`.
fn line_at(text: &str, offset: usize) -> usize {
    let end = offset.min(text.len());
    1 + text.as_bytes()[..end]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

/// The line of every key in a TOML limiter section, from toml's spans. The
/// text must already have parsed, so a parse error here cannot happen and
/// gives an empty map.
fn toml_map(text: &str) -> SourceMap {
    let mut map = SourceMap::default();
    let Ok(root) = DeTable::parse(text) else {
        return map;
    };
    for (key, value) in root.get_ref() {
        let line = line_at(text, key.span().start);
        if key.get_ref().as_ref() != "listeners" {
            map.top.set(key.get_ref(), line);
            continue;
        }
        let DeValue::Array(items) = value.get_ref() else {
            continue;
        };
        for item in items {
            let DeValue::Table(table) = item.get_ref() else {
                continue;
            };
            // A `[[listeners]]` item opens at its header, an inline table
            // at its brace.
            let first = table
                .keys()
                .map(|k| k.span().start)
                .min()
                .unwrap_or(item.span().start);
            let before = text[..first].trim_end();
            let open = before
                .strip_suffix("[[listeners]]")
                .map_or(item.span().start, str::len);
            let mut lines = ListenerLines::new(line_at(text, open));
            for k in table.keys() {
                lines.set(k.get_ref(), line_at(text, k.span().start));
            }
            map.listeners.push(lines);
        }
    }
    map
}

fn load(path: &str) -> Result<(LimiterConfig, SourceMap), String> {
    if path.ends_with(".toml") {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        let cfg: LimiterConfig =
            toml::from_str(&text).map_err(|e| format!("{path}: {}", e.to_string().trim_end()))?;
        let map = toml_map(&text);
        Ok((cfg, map))
    } else {
        read_pacerd_mapped(std::path::Path::new(path)).map_err(|e| format!("{path}: {e}"))
    }
}

/// Writes the config at `path` as TOML.
fn print(path: &str) -> ExitCode {
    let text = load(path).and_then(|(cfg, _)| {
        toml::to_string(&cfg).map_err(|e| format!("{path}: {}", e.to_string().trim_end()))
    });
    match text {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("edgefront: {e}");
            ExitCode::from(2)
        }
    }
}

/// Runs `edgefront check` with the arguments after `check`.
pub fn run(args: &[String]) -> ExitCode {
    if let [flag, path] = args
        && flag == "--print"
        && !path.starts_with("--")
    {
        return print(path);
    }
    let mut allowed = Vec::new();
    let mut rest = args;
    let path = loop {
        match rest {
            [flag, class, tail @ ..] if flag == "--allow" => {
                let Some(c) = Class::from_name(class) else {
                    let names: Vec<_> = Class::ALL.iter().map(|c| c.as_str()).collect();
                    eprintln!(
                        "edgefront: unknown lint class \"{class}\"; the classes are {}",
                        names.join(", ")
                    );
                    return ExitCode::from(2);
                };
                allowed.push(c);
                rest = tail;
            }
            [path] if !path.starts_with("--") => break path.as_str(),
            _ => {
                eprint!("{USAGE}");
                return ExitCode::from(2);
            }
        }
    };
    let http = match http_section(path) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("edgefront: {e}");
            return ExitCode::from(2);
        }
    };
    let mut found: Vec<(usize, String)> = Vec::new();
    if let Some(h) = &http {
        println!(
            "http_limits: {} rule{}",
            h.rules,
            if h.rules == 1 { "" } else { "s" }
        );
        found.extend(
            h.lints
                .iter()
                .filter(|(_, l)| !allowed.contains(&l.class))
                .map(|(line, l)| (*line, l.to_string())),
        );
        if !h.listeners {
            return verdict(path, &found);
        }
    }
    let (cfg, map) = match load(path) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("edgefront: {e}");
            return ExitCode::from(2);
        }
    };
    match crate::limits::address::load(std::path::Path::new(path)) {
        Ok(Some((_, summary))) => print!("{summary}"),
        Ok(None) => {}
        Err(e) => {
            eprintln!("edgefront: {path}: {e}");
            return ExitCode::from(2);
        }
    }
    found.extend(
        lint(&cfg, &map)
            .into_iter()
            .filter(|l| !allowed.contains(&l.class))
            .map(|l| (l.line, l.to_string())),
    );
    found.sort_by_key(|(line, _)| *line);
    verdict(path, &found)
}

/// Prints the findings and the verdict line.
fn verdict(path: &str, found: &[(usize, String)]) -> ExitCode {
    for (line, l) in found {
        println!("{path}:{line}: {l}");
    }
    match found.len() {
        0 => {
            println!("{path}: ok");
            ExitCode::SUCCESS
        }
        1 => {
            println!("{path}: 1 warning");
            ExitCode::FAILURE
        }
        n => {
            println!("{path}: {n} warnings");
            ExitCode::FAILURE
        }
    }
}

/// What `edgefront check` found in a TOML file's `[[http_limits]]`.
struct HttpCheck {
    rules: usize,
    /// Each finding with the line of its rule's `[[http_limits]]`.
    lints: Vec<(usize, edgefront_limiter::config::http_limits::HttpLint)>,
    /// Whether the file also has a limiter section.
    listeners: bool,
}

/// The HTTP rules of the TOML file at `path`, built and linted; `None`
/// for a pacerd.conf or a TOML file without the section.
fn http_section(path: &str) -> Result<Option<HttpCheck>, String> {
    use edgefront_limiter::config::http_limits::{self, HttpLimitsSection};
    if !path.ends_with(".toml") {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let section: HttpLimitsSection =
        toml::from_str(&text).map_err(|e| format!("{path}: {}", e.to_string().trim_end()))?;
    if section.http_limits.is_empty() {
        return Ok(None);
    }
    let rules = http_limits::build(&section.http_limits).map_err(|e| format!("{path}: {e}"))?;
    // Each rule's line: the n-th `[[http_limits]]` header.
    let heads: Vec<usize> = text
        .lines()
        .enumerate()
        .filter(|(_, l)| l.trim() == "[[http_limits]]")
        .map(|(i, _)| i + 1)
        .collect();
    let lints = http_limits::lint_configs(&section.http_limits, &rules)
        .into_iter()
        .map(|l| (heads.get(l.rule).copied().unwrap_or(0), l))
        .collect();
    let listeners = DeTable::parse(&text)
        .map(|t| t.get_ref().contains_key("listeners"))
        .unwrap_or(false);
    Ok(Some(HttpCheck {
        rules: rules.len(),
        lints,
        listeners,
    }))
}
