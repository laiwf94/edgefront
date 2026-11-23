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
//! An envoyproxy/ratelimit config is checked as the decision service
//! would load it (`edgefront_limiter::service::rls_config`): a `.yaml` or
//! `.yml` file alone, a directory as the service's config directory
//! (every file under it), or the files a TOML config's `[rls]` section
//! points at. The service's own refusals are a config that does not parse
//! (exit 2); otherwise a line with what loaded comes first, then the
//! `rls-*` findings, each on its file's line:
//!
//! ```text
//! rls: 2 files, 1 domain, 9 descriptors, 6 limits, 1 unlimited, 1 in shadow mode
//! /srv/runtime_data/current/ratelimit/config/api.yaml:14: warning[rls-zero-limit]: descriptor api.path_/login: ...
//! ```
//!
//! A TOML file with an `[rls]` section and neither listeners nor HTTP rules
//! is checked as that alone.
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
use edgefront_limiter::service::rls_config::{self, RlsLint, RlsSection};
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
    if path.ends_with(".yaml") || path.ends_with(".yml") || std::path::Path::new(path).is_dir() {
        return match rls_files(path) {
            Ok(extra) => verdict(path, &[], &filter(extra, &allowed)),
            Err(e) => {
                eprintln!("edgefront: {e}");
                ExitCode::from(2)
            }
        };
    }
    let rls = match rls_section(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("edgefront: {e}");
            return ExitCode::from(2);
        }
    };
    let extra = filter(
        rls.as_ref().map_or_else(Vec::new, |r| r.0.clone()),
        &allowed,
    );
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
            return verdict(path, &found, &extra);
        }
    }
    if let Some((_, listeners)) = &rls
        && !listeners
    {
        return verdict(path, &found, &extra);
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
    verdict(path, &found, &extra)
}

/// Prints the findings, in `path` by line and then `extra`'s, each with
/// its own file, and the verdict line.
fn verdict(path: &str, found: &[(usize, String)], extra: &[RlsLint]) -> ExitCode {
    for (line, l) in found {
        println!("{path}:{line}: {l}");
    }
    for l in extra {
        println!("{}:{}: {l}", l.file, l.line);
    }
    match found.len() + extra.len() {
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

/// The findings whose class is not allowed.
fn filter(lints: Vec<RlsLint>, allowed: &[Class]) -> Vec<RlsLint> {
    lints
        .into_iter()
        .filter(|l| !allowed.contains(&l.class))
        .collect()
}

/// Loads envoyproxy/ratelimit files, prints what loaded and returns the
/// findings, each named by `name(file)`.
fn rls_report(
    files: &[rls_config::ConfigFile],
    merge: bool,
    name: impl Fn(&str) -> String,
) -> Result<Vec<RlsLint>, String> {
    let loaded = rls_config::load(files, merge).map_err(|e| {
        // The service names a file by its path under the config directory;
        // here the whole path is clearer.
        match files
            .iter()
            .find(|f| e.starts_with(&format!("{}: ", f.name)))
        {
            Some(f) => format!("{}{}", name(&f.name), &e[f.name.len()..]),
            None => e,
        }
    })?;
    println!("rls: {}", loaded.stats);
    Ok(loaded
        .lints
        .into_iter()
        .map(|mut l| {
            l.file = name(&l.file);
            l
        })
        .collect())
}

/// A `.yaml` or `.yml` file alone, or a directory as the service's config
/// directory.
fn rls_files(path: &str) -> Result<Vec<RlsLint>, String> {
    let p = std::path::Path::new(path);
    if p.is_dir() {
        let files = rls_config::read_tree(p, "", false)?;
        return rls_report(&files, false, |n| p.join(n).display().to_string());
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let files = [rls_config::ConfigFile {
        name: path.to_owned(),
        text,
    }];
    rls_report(&files, false, str::to_owned)
}

/// The `[rls]` section of the TOML file at `path`, loaded: its findings,
/// and whether the file also has a limiter section. `None` for a
/// pacerd.conf or a TOML file without the section.
fn rls_section(path: &str) -> Result<Option<(Vec<RlsLint>, bool)>, String> {
    if !path.ends_with(".toml") {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let section: RlsSection =
        toml::from_str(&text).map_err(|e| format!("{path}: {}", e.to_string().trim_end()))?;
    let Some(settings) = section.rls else {
        return Ok(None);
    };
    let base = std::path::Path::new(path)
        .parent()
        .unwrap_or(std::path::Path::new("."));
    let settings = settings.relative_to(base);
    let files = settings
        .read_files()
        .map_err(|e| format!("{path}: rls: {e}"))?;
    let dir = settings.base_dir();
    let lints = rls_report(&files, settings.merge_domain_config, |n| {
        dir.join(n).display().to_string()
    })?;
    let listeners = DeTable::parse(&text)
        .map(|t| t.get_ref().contains_key("listeners"))
        .unwrap_or(false);
    Ok(Some((lints, listeners)))
}
