//! The `[address_policy]` section: the trusted proxies, the forwarding
//! header, and the classes with their prefix lists, with list files
//! resolved beside the config, built into an [`AddressPolicy`].
//!
//! ```toml
//! [address_policy]
//! header = "x-forwarded-for"            # or "forwarded"; none: the peer is the client
//! trusted_proxies = ["10.0.0.1", "192.0.2.0/24"]
//! trusted_proxy_files = ["proxies.txt"]
//! ipv6_prefix = 64                      # the default grouping
//! table_size = 16384                    # per limited class
//!
//! [[address_policy.classes]]
//! name = "partners"
//! action = "limit"                      # allow, deny or limit
//! prefixes = ["203.0.113.0/24"]
//! files = ["partners.txt"]              # one prefix per line, # comments
//! ipv6_prefix = 48
//! per_ip_max_conns = 50
//! per_ip_conn_rate = "100/s burst 200"
//! ```
//!
//! A class sets connection limits only; byte rates stay the listener's,
//! and `per_ip_bandwidth` in a class is refused rather than ignored.
//!
//! The section deserializes with serde from wherever the config is read
//! (`edgefront limits` reads it from the TOML file beside the limiter
//! section). [`AddressPolicyConfig::load`] reads the files again each time, so a
//! reload is `load` then [`crate::cidr::Swap::store`] of the new policy;
//! the old one keeps answering until then. [`Summary`] is what `edgefront
//! check` prints for it: each list's size and the memory the built policy
//! takes.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::cidr::{Prefix, PrefixList};
use crate::class::{Action, Class, Classifier, Limits};
use crate::client_addr::Header;
use crate::key::Grouping;
use crate::policy::AddressPolicy;

/// The section as written. Unknown keys are refused.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AddressPolicyConfig {
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    #[serde(default)]
    pub trusted_proxy_files: Vec<PathBuf>,
    #[serde(default)]
    pub ipv6_prefix: Option<u8>,
    #[serde(default)]
    pub table_size: Option<u32>,
    #[serde(default)]
    pub classes: Vec<ClassConfig>,
}

/// One class as written.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ClassConfig {
    pub name: String,
    pub action: String,
    #[serde(default)]
    pub prefixes: Vec<String>,
    #[serde(default)]
    pub files: Vec<PathBuf>,
    #[serde(default)]
    pub ipv6_prefix: Option<u8>,
    #[serde(default)]
    pub per_ip_max_conns: Option<u32>,
    #[serde(default)]
    pub per_ip_conn_rate: Option<String>,
    /// Refused with a reason; see the module docs.
    #[serde(default)]
    pub per_ip_bandwidth: Option<serde::de::IgnoredAny>,
}

/// Sizes of a loaded policy, for `edgefront check`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub trusted_proxies: usize,
    /// `(class, action, prefixes)` in config order.
    pub classes: Vec<(String, String, usize)>,
    /// `(file, prefixes)` for every list file read.
    pub files: Vec<(PathBuf, usize)>,
    pub prefixes: usize,
    pub memory: usize,
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "address_policy: {} trusted proxies, {} classes, {} prefixes, {} bytes",
            self.trusted_proxies,
            self.classes.len(),
            self.prefixes,
            self.memory
        )?;
        for (name, action, n) in &self.classes {
            writeln!(f, "  class {name}: {action}, {n} prefixes")?;
        }
        for (file, n) in &self.files {
            writeln!(f, "  list {}: {n} prefixes", file.display())?;
        }
        Ok(())
    }
}

/// The section's TOML key in an edgefront config.
pub const SECTION: &str = "address_policy";

fn read_list(
    dir: &Path,
    file: &Path,
    out: &mut Vec<(PathBuf, usize)>,
) -> Result<Vec<Prefix>, String> {
    let path = if file.is_absolute() {
        file.to_path_buf()
    } else {
        dir.join(file)
    };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let list = PrefixList::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    out.push((path, list.len()));
    Ok(list.iter().map(|(p, ())| p).collect())
}

fn prefix(field: &str, s: &str) -> Result<Prefix, String> {
    s.parse().map_err(|e| format!("{field}: {e}"))
}

impl AddressPolicyConfig {
    /// Reads every list file (relative paths from `dir`) and builds the
    /// policy, or names the first problem with its field or file.
    pub fn load(&self, dir: &Path) -> Result<(AddressPolicy, Summary), String> {
        let mut s = Summary::default();
        let header = match self.header.as_deref() {
            None => None,
            Some("forwarded") => Some(Header::Forwarded),
            Some("x-forwarded-for") => Some(Header::XForwardedFor),
            Some(h) => {
                return Err(format!(
                    "{SECTION}.header: {h:?} is not \"forwarded\" or \"x-forwarded-for\""
                ));
            }
        };
        let mut trusted = PrefixList::new();
        for p in &self.trusted_proxies {
            trusted.insert(prefix(&format!("{SECTION}.trusted_proxies"), p)?, ());
        }
        for f in &self.trusted_proxy_files {
            for p in read_list(dir, f, &mut s.files)? {
                trusted.insert(p, ());
            }
        }
        if header.is_none() && !trusted.is_empty() {
            return Err(format!(
                "{SECTION}: trusted proxies are set but no header names the client"
            ));
        }
        let default = match self.ipv6_prefix {
            None => Grouping::SLASH_64,
            Some(b) => Grouping::bits(b)
                .ok_or_else(|| format!("{SECTION}.ipv6_prefix: must be 1 to 128, got {b}"))?,
        };
        let mut classes = Vec::with_capacity(self.classes.len());
        for (i, c) in self.classes.iter().enumerate() {
            let at = |f: &str| format!("{SECTION}.classes[{i}].{f}");
            let action = match c.action.as_str() {
                "allow" => Action::Allow,
                "deny" => Action::Deny,
                "limit" => Action::Limit,
                a => {
                    return Err(format!(
                        "{}: {a:?} is not allow, deny or limit",
                        at("action")
                    ));
                }
            };
            let mut class = Class::new(c.name.clone(), action);
            class.ipv6_prefix = c.ipv6_prefix.unwrap_or(default.prefix_len());
            for p in &c.prefixes {
                class.prefixes.push(prefix(&at("prefixes"), p)?);
            }
            for f in &c.files {
                class.prefixes.extend(read_list(dir, f, &mut s.files)?);
            }
            if c.per_ip_bandwidth.is_some() {
                return Err(format!(
                    "{}: a class sets connection limits only; byte rates are the listener's per_ip_bandwidth",
                    at("per_ip_bandwidth")
                ));
            }
            let limits_set = c.per_ip_max_conns.is_some() || c.per_ip_conn_rate.is_some();
            if action != Action::Limit && limits_set {
                return Err(format!(
                    "{SECTION}.classes[{i}]: class {} is {}, so its limits would never apply",
                    c.name,
                    action.as_str()
                ));
            }
            class.limits = Limits {
                per_ip_max_conns: c.per_ip_max_conns,
                per_ip_conn_rate: c
                    .per_ip_conn_rate
                    .as_deref()
                    .map(|r| super::spec_rate("per_ip_conn_rate", r))
                    .transpose()
                    .map_err(|e| format!("{SECTION}.classes[{i}].{e}"))?,
            };
            s.classes.push((
                c.name.clone(),
                action.as_str().to_string(),
                class.prefixes.len(),
            ));
            classes.push(class);
        }
        let classifier = Classifier::new(classes).map_err(|e| format!("{SECTION}: {e}"))?;
        s.prefixes = classifier.prefixes();
        s.trusted_proxies = trusted.len();
        let policy = AddressPolicy::new(
            trusted,
            header,
            classifier,
            self.table_size.unwrap_or(16_384),
            default,
        )
        .map_err(|e| format!("{SECTION}.table_size: {e}"))?;
        s.memory = policy.memory();
        Ok((policy, s))
    }
}

#[cfg(test)]
mod tests;
