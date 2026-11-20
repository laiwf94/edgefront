//! Per-connection quotas for HTTP keep-alive and HTTP/2 connections: the
//! `[connection_quota]` section of a limiter config.
//!
//! ```toml
//! [connection_quota]
//! max_requests = 1000     # requests on one connection
//! max_bytes = "1g"        # bytes read and written on it, k/m/g suffixes
//! ```
//!
//! A long-lived connection is one client's pipe that no per-request limit
//! can make it close, and over HTTP/2 one connection can carry a client's
//! whole flood. With a quota, the request that reaches it is the last:
//! its HTTP/1.1 response carries `Connection: close`, an HTTP/2
//! connection is sent GOAWAY so the client moves its next streams to a
//! new connection (which the listener's connection limits then see), and
//! what is already in flight finishes. The mechanism is the listener's
//! ([`edgefront_core::listener::ConnQuota`], counted per connection);
//! this module reads and checks the policy, and `edgefront proxy
//! --limits` applies it with `Proxy::with_conn_quota`.

use edgefront_core::listener::ConnQuota;
use serde::Deserialize;

/// The section as written.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConnQuotaConfig {
    #[serde(default)]
    pub max_requests: Option<u64>,
    /// Bytes, with an optional `k`, `m` or `g` (powers of 1024).
    #[serde(default)]
    pub max_bytes: Option<crate::config::http_limits::StatusItem>,
}

/// A document's `connection_quota` table, ignoring everything else.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ConnQuotaSection {
    #[serde(default)]
    pub connection_quota: Option<ConnQuotaConfig>,
}

fn bytes(text: &str) -> Option<u64> {
    let t = text.trim().to_ascii_lowercase();
    let (num, mul) = match t.as_bytes().last()? {
        b'k' => (&t[..t.len() - 1], 1u64 << 10),
        b'm' => (&t[..t.len() - 1], 1 << 20),
        b'g' => (&t[..t.len() - 1], 1 << 30),
        _ => (t.as_str(), 1),
    };
    num.parse::<u64>().ok()?.checked_mul(mul)
}

impl ConnQuotaConfig {
    /// The quota, or the mistake as `connection_quota.field: reason`.
    pub fn build(&self) -> Result<ConnQuota, String> {
        use crate::config::http_limits::StatusItem;
        if self.max_requests == Some(0) {
            return Err("connection_quota.max_requests: must be at least 1".into());
        }
        let max_bytes = match &self.max_bytes {
            None => None,
            Some(StatusItem::Code(n)) => Some(*n),
            Some(StatusItem::Text(t)) => Some(bytes(t).ok_or_else(|| {
                format!(
                    "connection_quota.max_bytes: {t:?} is not a size like 65536, 512k, 64m or 1g"
                )
            })?),
        };
        if max_bytes == Some(0) {
            return Err("connection_quota.max_bytes: must be at least 1".into());
        }
        if self.max_requests.is_none() && max_bytes.is_none() {
            return Err("connection_quota: set max_requests, max_bytes or both".into());
        }
        Ok(ConnQuota {
            max_requests: self.max_requests,
            max_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quota(text: &str) -> Result<Option<ConnQuota>, String> {
        let s: ConnQuotaSection = toml::from_str(text).map_err(|e| e.to_string())?;
        s.connection_quota.map(|c| c.build()).transpose()
    }

    #[test]
    fn the_section_reads_and_checks() {
        assert_eq!(quota("").unwrap(), None);
        assert_eq!(
            quota("[connection_quota]\nmax_requests = 1000\nmax_bytes = \"1g\"\n").unwrap(),
            Some(ConnQuota {
                max_requests: Some(1000),
                max_bytes: Some(1 << 30)
            })
        );
        assert_eq!(
            quota("[connection_quota]\nmax_bytes = 4096\n").unwrap(),
            Some(ConnQuota {
                max_requests: None,
                max_bytes: Some(4096)
            })
        );
        assert_eq!(
            quota("[connection_quota]\nmax_bytes = \"512k\"\n")
                .unwrap()
                .unwrap()
                .max_bytes,
            Some(512 << 10)
        );
        assert_eq!(
            quota("[connection_quota]\nmax_bytes = \"64M\"\n")
                .unwrap()
                .unwrap()
                .max_bytes,
            Some(64 << 20)
        );
        for (text, want) in [
            (
                "[connection_quota]\n",
                "connection_quota: set max_requests, max_bytes or both",
            ),
            (
                "[connection_quota]\nmax_requests = 0\n",
                "connection_quota.max_requests: must be at least 1",
            ),
            (
                "[connection_quota]\nmax_bytes = 0\n",
                "connection_quota.max_bytes: must be at least 1",
            ),
            (
                "[connection_quota]\nmax_bytes = \"lots\"\n",
                "connection_quota.max_bytes: \"lots\" is not a size",
            ),
            (
                "[connection_quota]\nmax_bytes = \"99999999999g\"\n",
                "is not a size",
            ),
            (
                "[connection_quota]\nmax_conns = 1\n",
                "unknown field `max_conns`",
            ),
        ] {
            let e = quota(text).unwrap_err();
            assert!(e.contains(want), "{text}: {e}");
        }
    }
}
