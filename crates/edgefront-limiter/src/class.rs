//! Address classes: prefix lists bound to an action and a set of limits.
//!
//! A [`Class`] names prefixes (from lists, [`crate::cidr::PrefixList`]),
//! an [`Action`] and, for [`Action::Limit`], the limits its clients get
//! instead of the listener's own: connection cap, connection rate and the
//! IPv6 prefix clients are grouped by. Byte rates stay the listener's. A [`Classifier`] puts
//! every class's prefixes into one prefix list and answers, for an
//! address, the class of the most specific prefix that contains it, so a
//! partner's /24 inside a denied /16 is the partner's. An address no
//! class covers gets the default: the listener's own limits.
//!
//! One prefix may belong to one class only: a prefix listed by two
//! classes is refused when the classifier is built, naming both, since
//! either answer would be a guess. A prefix listed twice by one class is
//! one prefix.

use std::fmt;
use std::net::IpAddr;

use crate::cidr::{Prefix, PrefixList};
use crate::gcra::Rate;

/// What happens to a client of the class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// Admitted with no per-client limit; the listener's own cap and
    /// bandwidth still apply.
    Allow,
    /// Refused at once.
    Deny,
    /// Admitted under the class's limits.
    Limit,
}

impl Action {
    pub const fn as_str(self) -> &'static str {
        match self {
            Action::Allow => "allow",
            Action::Deny => "deny",
            Action::Limit => "limit",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The per-client limits a class sets. A limit left `None` is off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Limits {
    pub per_ip_max_conns: Option<u32>,
    pub per_ip_conn_rate: Option<Rate>,
}

/// One class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Class {
    pub name: String,
    pub action: Action,
    pub limits: Limits,
    /// The IPv6 prefix its clients are grouped by: 64, 56 or 48 bits, or
    /// any length from 1 to 128.
    pub ipv6_prefix: u8,
    pub prefixes: Vec<Prefix>,
}

impl Class {
    /// A class with no prefixes, no limits and /64 grouping.
    pub fn new(name: impl Into<String>, action: Action) -> Self {
        Self {
            name: name.into(),
            action,
            limits: Limits::default(),
            ipv6_prefix: 64,
            prefixes: Vec::new(),
        }
    }
}

/// Every class's prefixes in one list. See the [module docs](self).
#[derive(Clone, Debug)]
pub struct Classifier {
    classes: Vec<Class>,
    list: PrefixList<u16>,
}

/// The class an address belongs to, with the prefix that put it there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Classified<'a> {
    pub class: &'a Class,
    pub index: usize,
    pub prefix: Prefix,
}

impl Classifier {
    /// Builds the classifier, or names the first problem: two classes
    /// with one name, a class with no prefixes, a grouping outside 1 to
    /// 128, a prefix in two classes, or more than 65,535 classes.
    pub fn new(classes: Vec<Class>) -> Result<Self, String> {
        if classes.len() > usize::from(u16::MAX) {
            return Err(format!("{} classes, at most 65535", classes.len()));
        }
        let mut list = PrefixList::new();
        for (i, c) in classes.iter().enumerate() {
            if classes[..i].iter().any(|o| o.name == c.name) {
                return Err(format!("class {} is defined twice", c.name));
            }
            if !(1..=128).contains(&c.ipv6_prefix) {
                return Err(format!(
                    "class {}: ipv6_prefix must be 1 to 128, got {}",
                    c.name, c.ipv6_prefix
                ));
            }
            if c.prefixes.is_empty() {
                return Err(format!("class {}: no prefixes", c.name));
            }
            for p in &c.prefixes {
                if let Some(&other) = list.get(p)
                    && usize::from(other) != i
                {
                    return Err(format!(
                        "{p} is in classes {} and {}",
                        classes[usize::from(other)].name,
                        c.name
                    ));
                }
                list.insert(*p, i as u16);
            }
        }
        list.compile();
        Ok(Self { classes, list })
    }

    /// No classes: every address gets the default.
    pub fn empty() -> Self {
        Self {
            classes: Vec::new(),
            list: PrefixList::new(),
        }
    }

    /// The class of the most specific prefix containing `ip`, or `None`
    /// for the default.
    #[inline]
    pub fn classify(&self, ip: IpAddr) -> Option<Classified<'_>> {
        self.list.lookup(ip).map(|m| Classified {
            class: &self.classes[usize::from(*m.value)],
            index: usize::from(*m.value),
            prefix: m.prefix,
        })
    }

    pub fn classes(&self) -> &[Class] {
        &self.classes
    }

    /// Prefixes across every class.
    pub fn prefixes(&self) -> usize {
        self.list.len()
    }

    /// Bytes the classifier's prefix list takes.
    pub fn memory(&self) -> usize {
        self.list.memory()
    }
}

#[cfg(test)]
mod tests;
