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
    let (cfg, map) = match load(path) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("edgefront: {e}");
            return ExitCode::from(2);
        }
    };
    let found: Vec<_> = lint(&cfg, &map)
        .into_iter()
        .filter(|l| !allowed.contains(&l.class))
        .collect();
    for l in &found {
        println!("{path}:{}: {l}", l.line);
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
