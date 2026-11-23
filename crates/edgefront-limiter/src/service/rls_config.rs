//! An envoyproxy/ratelimit deployment's config, loaded unchanged: the YAML
//! files the service reads from its runtime directory, the settings that
//! say where they are and how they combine, and the checks `edgefront
//! check` runs on them.
//!
//! **Settings** ([`RlsSettings`]) are the service's own, under the names
//! of its environment variables, so a deployment moves over by copying
//! them: from the environment with [`RlsSettings::from_env`], or from an
//! edgefront config's `[rls]` section ([`RlsSection`]):
//!
//! ```toml
//! [rls]
//! runtime_root = "/srv/runtime_data/current"   # RUNTIME_ROOT
//! runtime_subdirectory = "ratelimit"           # RUNTIME_SUBDIRECTORY
//! runtime_app_directory = "config"             # RUNTIME_APPDIRECTORY
//! runtime_ignore_dotfiles = false              # RUNTIME_IGNOREDOTFILES
//! merge_domain_config = false                  # MERGE_DOMAIN_CONFIG
//! shadow_mode = false                          # SHADOW_MODE
//! ```
//!
//! Every key is optional and defaults as the service does. A relative
//! `runtime_root` is taken from the directory of the file that names it.
//!
//! **Files** ([`RlsSettings::read_files`]) are every regular file under
//! `<runtime_root>/<runtime_subdirectory>/<runtime_app_directory>`, at any
//! depth and whatever its extension, as the service's runtime loader
//! reads them; with `runtime_ignore_dotfiles` a file or directory whose
//! name starts with `.` is skipped, which keeps editors' swap files and a
//! symlink swap's `..data` out. Symbolic links are followed, since the
//! usual deployment swaps `current` from one release directory to the
//! next. They load in path order, so of two files with one domain the
//! later one is refused (or merged into the earlier one with
//! `merge_domain_config`), and each is named by its path under the
//! subdirectory, `config/api.yaml`, in every message about it.
//!
//! **Loading** ([`load`]) builds the descriptor trees with
//! [`DescriptorConfig::load`], which takes `shadow_mode`, `replaces` and
//! `unlimited` as the service does and refuses what it refuses with its
//! messages. Global `shadow_mode` is not part of the trees: the decision
//! service applies it to whole responses (`RateLimitService::
//! with_global_shadow_mode`).
//!
//! **Lints** ([`lint`]) find what loads but cannot do what it says, each
//! with its file and line:
//!
//! | class | finding |
//! |---|---|
//! | `rls-zero-limit` | an enforcing limit allows 0 requests per unit (often a missing `requests_per_unit`), so every request it matches is over the limit |
//! | `rls-replaces-unknown` | a `replaces` entry names no limit in the domain, so it replaces nothing |
//! | `rls-shadow-no-limit` | `shadow_mode` is set on a descriptor with no limit or an unlimited one, so there is nothing to shadow |
//! | `rls-wildcard-shadowed` | a descriptor is never reached: an earlier wildcard value for the same key matches every value it would, or a wildcard of only `*` takes every value from a descriptor with no value |

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::descriptor::{DescriptorConfig, Node, parse_yaml, wildcard_match};
use crate::lint::Class;

/// The service's default `RUNTIME_ROOT`.
pub const DEFAULT_RUNTIME_ROOT: &str = "/srv/runtime_data/current";

/// The service's default `RUNTIME_APPDIRECTORY`.
pub const DEFAULT_APP_DIRECTORY: &str = "config";

/// How deep [`RlsSettings::read_files`] goes below the config directory:
/// far past any real layout, and a stop for a symbolic link loop.
const MAX_DEPTH: usize = 32;

/// Where an envoyproxy/ratelimit deployment keeps its config and how the
/// files combine, under the service's own names. See the module docs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RlsSettings {
    pub runtime_root: PathBuf,
    pub runtime_subdirectory: String,
    pub runtime_app_directory: String,
    pub runtime_ignore_dotfiles: bool,
    /// Two files with one domain join their descriptors instead of the
    /// second being refused.
    pub merge_domain_config: bool,
    /// Every response is `OK`, whatever its descriptors say.
    pub shadow_mode: bool,
}

impl Default for RlsSettings {
    fn default() -> Self {
        RlsSettings {
            runtime_root: PathBuf::from(DEFAULT_RUNTIME_ROOT),
            runtime_subdirectory: String::new(),
            runtime_app_directory: DEFAULT_APP_DIRECTORY.to_owned(),
            runtime_ignore_dotfiles: false,
            merge_domain_config: false,
            shadow_mode: false,
        }
    }
}

/// A TOML document's `[rls]` section, ignoring every other section.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct RlsSection {
    #[serde(default)]
    pub rls: Option<RlsSettings>,
}

/// A boolean as the service's settings read one (Go's `strconv.ParseBool`).
fn go_bool(name: &str, text: &str) -> Result<bool, String> {
    match text {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        _ => Err(format!("{name}: invalid boolean '{text}'")),
    }
}

impl RlsSettings {
    /// The settings from the service's environment variables, looked up
    /// with `var`: `RUNTIME_ROOT`, `RUNTIME_SUBDIRECTORY`,
    /// `RUNTIME_APPDIRECTORY`, `RUNTIME_IGNOREDOTFILES`,
    /// `MERGE_DOMAIN_CONFIG` and `SHADOW_MODE`. An unset variable keeps its
    /// default; a boolean that Go would not parse is an error.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mut s = RlsSettings::default();
        if let Some(v) = var("RUNTIME_ROOT") {
            s.runtime_root = PathBuf::from(v);
        }
        if let Some(v) = var("RUNTIME_SUBDIRECTORY") {
            s.runtime_subdirectory = v;
        }
        if let Some(v) = var("RUNTIME_APPDIRECTORY") {
            s.runtime_app_directory = v;
        }
        let flag = |name: &str, out: &mut bool| -> Result<(), String> {
            if let Some(v) = var(name) {
                *out = go_bool(name, &v)?;
            }
            Ok(())
        };
        flag("RUNTIME_IGNOREDOTFILES", &mut s.runtime_ignore_dotfiles)?;
        flag("MERGE_DOMAIN_CONFIG", &mut s.merge_domain_config)?;
        flag("SHADOW_MODE", &mut s.shadow_mode)?;
        Ok(s)
    }

    /// The settings with a relative `runtime_root` taken from `base`, the
    /// directory of the file that set it.
    #[must_use]
    pub fn relative_to(mut self, base: &Path) -> Self {
        if self.runtime_root.is_relative() {
            self.runtime_root = base.join(&self.runtime_root);
        }
        self
    }

    /// The directory the files' names are relative to.
    pub fn base_dir(&self) -> PathBuf {
        self.runtime_root.join(&self.runtime_subdirectory)
    }

    /// The directory holding the files.
    pub fn config_dir(&self) -> PathBuf {
        self.base_dir().join(&self.runtime_app_directory)
    }

    /// Every config file, in path order. See the module docs.
    pub fn read_files(&self) -> Result<Vec<ConfigFile>, String> {
        let dir = self.config_dir();
        let prefix = if self.runtime_app_directory.is_empty() {
            String::new()
        } else {
            format!("{}/", self.runtime_app_directory.trim_end_matches('/'))
        };
        read_tree(&dir, &prefix, self.runtime_ignore_dotfiles)
    }
}

/// One config file: its name in messages and its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigFile {
    pub name: String,
    pub text: String,
}

/// Every regular file under `dir`, named `prefix` and its path under
/// `dir`, in path order, skipping dot names when `ignore_dotfiles`.
pub fn read_tree(
    dir: &Path,
    prefix: &str,
    ignore_dotfiles: bool,
) -> Result<Vec<ConfigFile>, String> {
    let mut out = Vec::new();
    walk(dir, prefix, ignore_dotfiles, 0, &mut out)?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn walk(
    dir: &Path,
    prefix: &str,
    ignore_dotfiles: bool,
    depth: usize,
    out: &mut Vec<ConfigFile>,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!(
            "{}: more than {MAX_DEPTH} directories deep, a symbolic link loop?",
            dir.display()
        ));
    }
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            return Err(format!(
                "{}: a file name that is not UTF-8: {:?}",
                dir.display(),
                entry.file_name()
            ));
        };
        if ignore_dotfiles && file_name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        // Through symbolic links: a link to a file is that file.
        let meta =
            std::fs::metadata(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let name = format!("{prefix}{file_name}");
        if meta.is_dir() {
            walk(&path, &format!("{name}/"), ignore_dotfiles, depth + 1, out)?;
        } else if meta.is_file() {
            let bytes =
                std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let text = String::from_utf8(bytes)
                .map_err(|_| format!("{name}: error loading config file: not UTF-8"))?;
            out.push(ConfigFile { name, text });
        }
    }
    Ok(())
}

/// What a set of files holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RlsStats {
    pub files: usize,
    pub domains: usize,
    /// Descriptors at every depth.
    pub descriptors: usize,
    /// Descriptors with a limit that counts, unlimited ones aside.
    pub limits: usize,
    pub unlimited: usize,
    /// Descriptors in shadow mode.
    pub shadow: usize,
}

impl fmt::Display for RlsStats {
    /// `3 files, 2 domains, 12 descriptors, 7 limits, 1 unlimited, 2 in shadow mode`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n =
            |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        write!(
            f,
            "{}, {}, {}, {}, {} unlimited, {} in shadow mode",
            n(self.files, "file", "files"),
            n(self.domains, "domain", "domains"),
            n(self.descriptors, "descriptor", "descriptors"),
            n(self.limits, "limit", "limits"),
            self.unlimited,
            self.shadow
        )
    }
}

/// A loaded deployment.
#[derive(Debug)]
pub struct LoadedRls {
    pub config: DescriptorConfig,
    /// The files' names, in load order.
    pub files: Vec<String>,
    pub stats: RlsStats,
    pub lints: Vec<RlsLint>,
}

/// Loads and lints `files`, in order; `merge` is `merge_domain_config`.
/// The error is the service's message for the first file it refuses.
pub fn load(files: &[ConfigFile], merge: bool) -> Result<LoadedRls, String> {
    let pairs: Vec<(&str, &str)> = files
        .iter()
        .map(|f| (f.name.as_str(), f.text.as_str()))
        .collect();
    let config = DescriptorConfig::load(&pairs, merge)?;
    let (stats, lints) = lint(&pairs)?;
    Ok(LoadedRls {
        config,
        files: files.iter().map(|f| f.name.clone()).collect(),
        stats,
        lints,
    })
}

/// Loads a deployment as `settings` describe it.
pub fn load_settings(settings: &RlsSettings) -> Result<LoadedRls, String> {
    load(&settings.read_files()?, settings.merge_domain_config)
}

/// One warning about a descriptor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RlsLint {
    /// The file's name and the 1-based line.
    pub file: String,
    pub line: usize,
    pub class: Class,
    /// The descriptor's full key, `domain.key_value.subkey`.
    pub descriptor: String,
    pub message: String,
}

impl fmt::Display for RlsLint {
    /// `warning[CLASS]: descriptor KEY: MESSAGE`; the caller puts the file
    /// and line in front.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "warning[{}]: descriptor {}: {}",
            self.class, self.descriptor, self.message
        )
    }
}

fn entries(n: &Node) -> &[(String, Node)] {
    match n {
        Node::Map { entries, .. } => entries,
        _ => &[],
    }
}

fn field<'n>(n: &'n Node, key: &str) -> Option<&'n Node> {
    entries(n)
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .filter(|v| !matches!(v, Node::Null { .. }))
}

fn text<'n>(n: &'n Node, key: &str) -> &'n str {
    field(n, key).and_then(Node::as_str).unwrap_or("")
}

fn flag(n: &Node, key: &str) -> bool {
    matches!(text(n, key), "true" | "True" | "TRUE")
}

/// The line of `key` in a map: its value's, or the map's when it is not
/// there.
fn line_of(n: &Node, key: &str) -> usize {
    entries(n)
        .iter()
        .find(|(k, _)| k == key)
        .map_or_else(|| n.line(), |(_, v)| v.line())
}

/// One descriptor of a level, with its file.
struct Item<'a> {
    file: &'a str,
    node: &'a Node,
}

/// Every domain's top-level descriptors across files, in load order.
type Domains<'a> = Vec<(&'a str, Vec<Item<'a>>)>;

/// The stats and lints of `files` (`(name, text)`), which must already
/// load. In file order, then line order within a file.
pub fn lint(files: &[(&str, &str)]) -> Result<(RlsStats, Vec<RlsLint>), String> {
    let mut docs = Vec::with_capacity(files.len());
    for (name, body) in files {
        let doc =
            parse_yaml(body).map_err(|e| format!("{name}: error loading config file: {e}"))?;
        docs.push((*name, doc));
    }
    let mut domains: Domains<'_> = Vec::new();
    for (name, doc) in &docs {
        let domain = text(doc, "domain");
        let at = match domains.iter().position(|(d, _)| *d == domain) {
            Some(i) => i,
            None => {
                domains.push((domain, Vec::new()));
                domains.len() - 1
            }
        };
        if let Some(Node::Seq { items, .. }) = field(doc, "descriptors") {
            domains[at]
                .1
                .extend(items.iter().map(|node| Item { file: name, node }));
        }
    }
    let mut stats = RlsStats {
        files: files.len(),
        domains: domains.len(),
        ..RlsStats::default()
    };
    let mut out = Vec::new();
    for (domain, top) in &domains {
        let mut names = Vec::new();
        collect_names(top, &mut names);
        level(domain, domain, top, &names, &mut stats, &mut out);
    }
    let order = |f: &str| {
        files
            .iter()
            .position(|(n, _)| *n == f)
            .unwrap_or(usize::MAX)
    };
    out.sort_by_key(|l| (order(&l.file), l.line));
    Ok((stats, out))
}

/// Every limit's name under `items`.
fn collect_names<'a>(items: &[Item<'a>], names: &mut Vec<&'a str>) {
    for it in items {
        if let Some(rl) = field(it.node, "rate_limit") {
            let n = text(rl, "name");
            if !n.is_empty() {
                names.push(n);
            }
        }
        if let Some(Node::Seq { items: sub, .. }) = field(it.node, "descriptors") {
            let sub: Vec<Item<'a>> = sub
                .iter()
                .map(|node| Item {
                    file: it.file,
                    node,
                })
                .collect();
            collect_names(&sub, names);
        }
    }
}

/// The first of `among` with `key` and a wildcard value `pred` holds for.
fn covering<'i, 'a>(
    among: &'i [Item<'a>],
    key: &str,
    pred: impl Fn(&str) -> bool,
) -> Option<&'i Item<'a>> {
    among.iter().find(|o| {
        let v = text(o.node, "value");
        text(o.node, "key") == key && v.contains('*') && pred(v)
    })
}

/// Whether a wildcard pattern matches every value: only `*`s.
fn matches_all(pattern: &str) -> bool {
    !pattern.is_empty() && pattern.bytes().all(|b| b == b'*')
}

fn level(
    domain: &str,
    parent: &str,
    items: &[Item<'_>],
    names: &[&str],
    stats: &mut RlsStats,
    out: &mut Vec<RlsLint>,
) {
    for (i, it) in items.iter().enumerate() {
        let node = it.node;
        let key = text(node, "key");
        let value = text(node, "value");
        let full_key = if value.is_empty() {
            format!("{parent}.{key}")
        } else {
            format!("{parent}.{key}_{value}")
        };
        stats.descriptors += 1;
        let mut warn = |line: usize, class: Class, message: String| {
            out.push(RlsLint {
                file: it.file.to_owned(),
                line,
                class,
                descriptor: full_key.clone(),
                message,
            });
        };
        // Matching tries the exact value, then wildcards in order, then
        // the key alone: an earlier wildcard covering this one, or one of
        // only `*` beside a key alone, takes every value first.
        let shadowed_by = if value.contains('*') {
            // A pattern's own text is a value it matches only through its
            // stars, so an earlier pattern that matches it matches every
            // value it does.
            covering(&items[..i], key, |v| wildcard_match(v, value))
        } else if value.is_empty() {
            covering(items, key, matches_all)
        } else {
            None
        };
        if let Some(o) = shadowed_by {
            let at = if o.file == it.file {
                format!("line {}", o.node.line())
            } else {
                format!("{} line {}", o.file, o.node.line())
            };
            warn(
                node.line(),
                Class::RlsWildcardShadowed,
                format!(
                    "never reached: the wildcard value '{}' on {at} matches every value this descriptor would, and is tried first",
                    text(o.node, "value")
                ),
            );
        }
        let rl = field(node, "rate_limit");
        let unlimited = rl.is_some_and(|rl| flag(rl, "unlimited"));
        let shadow = flag(node, "shadow_mode");
        if shadow {
            stats.shadow += 1;
        }
        match rl {
            Some(_) if unlimited => stats.unlimited += 1,
            Some(_) => stats.limits += 1,
            None => {}
        }
        if shadow && (rl.is_none() || unlimited) {
            warn(
                line_of(node, "shadow_mode"),
                Class::RlsShadowNoLimit,
                format!(
                    "shadow_mode is set but the descriptor {}, so there is nothing to shadow",
                    if unlimited {
                        "is unlimited"
                    } else {
                        "has no rate_limit"
                    }
                ),
            );
        }
        if let Some(rl) = rl {
            let rpu = text(rl, "requests_per_unit");
            if !unlimited && !shadow && matches!(rpu, "" | "0") {
                warn(
                    if rpu.is_empty() {
                        rl.line()
                    } else {
                        line_of(rl, "requests_per_unit")
                    },
                    Class::RlsZeroLimit,
                    format!(
                        "the limit allows 0 requests per {}{}, so every request it matches is over the limit",
                        text(rl, "unit").to_ascii_lowercase(),
                        if rpu.is_empty() {
                            " (requests_per_unit is missing)"
                        } else {
                            ""
                        }
                    ),
                );
            }
            if let Some(Node::Seq { items: reps, .. }) = field(rl, "replaces") {
                for r in reps {
                    let n = text(r, "name");
                    if !n.is_empty() && !names.contains(&n) {
                        warn(
                            r.line(),
                            Class::RlsReplacesUnknown,
                            format!(
                                "replaces '{n}', but no limit in domain {} has that name, so it replaces nothing",
                                domain
                            ),
                        );
                    }
                }
            }
        }
        if let Some(Node::Seq { items: sub, .. }) = field(node, "descriptors") {
            let sub: Vec<Item<'_>> = sub
                .iter()
                .map(|n| Item {
                    file: it.file,
                    node: n,
                })
                .collect();
            level(domain, &full_key, &sub, names, stats, out);
        }
    }
}

#[cfg(test)]
mod tests;
