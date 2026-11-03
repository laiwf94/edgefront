//! The config schema as written, before defaults and validation, and the
//! decoder that fills it from a [`Node`] tree with Go's `encoding/json`
//! rules, which steer applied with `DisallowUnknownFields`:
//!
//! - Field names match case-insensitively, with `ſ` (U+017F) matching `s`
//!   and the Kelvin sign (U+212A) matching `k`; unknown fields are errors.
//! - A duplicate key decodes into what the earlier one left: objects merge,
//!   and arrays decode element by element into the existing elements, then
//!   drop the rest.
//! - `null` clears an optional value or list, and leaves a plain string,
//!   number or boolean unchanged.
//! - Integers must be written as integers (`5`, not `5.0` or `5e0`); a
//!   float that overflows is an error, one that underflows is zero.
//! - Durations are strings in Go's `time.ParseDuration` syntax.

use super::go;
use super::node::Node;

/// A decode failure at a field path.
#[derive(Debug)]
pub(crate) struct DecodeError {
    pub(crate) path: String,
    pub(crate) message: String,
}

type Result<T = ()> = std::result::Result<T, DecodeError>;

fn fail(path: &str, message: impl Into<String>) -> DecodeError {
    DecodeError {
        path: if path.is_empty() {
            "(top level)".to_owned()
        } else {
            path.to_owned()
        },
        message: message.into(),
    }
}

fn mismatch(path: &str, node: &Node, want: &str) -> DecodeError {
    fail(path, format!("expected {want}, found {}", node.kind()))
}

fn field(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_owned()
    } else {
        format!("{path}.{name}")
    }
}

/// A key as Go compares it with a field name: ASCII case folded, plus the
/// two non-ASCII letters that fold to ASCII ones.
fn fold_key(key: &str) -> String {
    key.chars()
        .map(|c| match c {
            '\u{17F}' => 's',
            '\u{212A}' => 'k',
            c => c.to_ascii_lowercase(),
        })
        .collect()
}

/// A type the decoder can fill in place.
pub(crate) trait Decode: Default {
    fn decode(&mut self, node: &Node, path: &str) -> Result;
}

/// Decodes the members of an object into a struct, one field at a time.
fn members(
    node: &Node,
    path: &str,
    what: &str,
    mut set: impl FnMut(&str, &Node, &str) -> Option<Result>,
) -> Result {
    match node {
        Node::Null => Ok(()),
        Node::Object(members) => {
            for (key, value) in members {
                let name = fold_key(key);
                let at = field(path, &name);
                match set(&name, value, &at) {
                    Some(result) => result?,
                    None => return Err(fail(path, format!("unknown field {}", go::quote(key)))),
                }
            }
            Ok(())
        }
        other => Err(mismatch(path, other, what)),
    }
}

impl Decode for String {
    fn decode(&mut self, node: &Node, path: &str) -> Result {
        match node {
            Node::Null => Ok(()),
            Node::String(s) => {
                s.clone_into(self);
                Ok(())
            }
            other => Err(mismatch(path, other, "a string")),
        }
    }
}

impl Decode for bool {
    fn decode(&mut self, node: &Node, path: &str) -> Result {
        match node {
            Node::Null => Ok(()),
            Node::Bool(b) => {
                *self = *b;
                Ok(())
            }
            other => Err(mismatch(path, other, "true or false")),
        }
    }
}

impl Decode for i64 {
    fn decode(&mut self, node: &Node, path: &str) -> Result {
        match node {
            Node::Null => Ok(()),
            Node::Number(text) => {
                *self = text.parse().map_err(|_| {
                    let integral = !text.contains(['.', 'e', 'E']);
                    fail(
                        path,
                        if integral {
                            format!("{text} is outside the 64-bit integer range")
                        } else {
                            format!("expected an integer, found {text}")
                        },
                    )
                })?;
                Ok(())
            }
            other => Err(mismatch(path, other, "an integer")),
        }
    }
}

impl Decode for f64 {
    fn decode(&mut self, node: &Node, path: &str) -> Result {
        match node {
            Node::Null => Ok(()),
            Node::Number(text) => {
                *self = text
                    .parse::<f64>()
                    .ok()
                    .filter(|f| f.is_finite())
                    .ok_or_else(|| fail(path, format!("{text} is out of range for a number")))?;
                Ok(())
            }
            other => Err(mismatch(path, other, "a number")),
        }
    }
}

/// A duration in nanoseconds, written as a Go duration string.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Duration(pub(crate) i64);

impl Decode for Duration {
    fn decode(&mut self, node: &Node, path: &str) -> Result {
        match node {
            Node::String(s) => {
                self.0 = go::parse_duration(s)
                    .ok_or_else(|| fail(path, format!("invalid duration {}", go::quote(s))))?;
                Ok(())
            }
            _ => Err(fail(
                path,
                r#"durations are strings such as "250ms" or "5s""#,
            )),
        }
    }
}

/// A Go pointer: `null` clears it, anything else decodes into the value it
/// already holds or a fresh one.
impl<T: Decode> Decode for Option<T> {
    fn decode(&mut self, node: &Node, path: &str) -> Result {
        if let Node::Null = node {
            *self = None;
            return Ok(());
        }
        self.get_or_insert_with(T::default).decode(node, path)
    }
}

/// A Go slice. Wrapped in `Option` it tells an absent or `null` list from
/// an empty one, as a nil slice does.
impl<T: Decode> Decode for Vec<T> {
    fn decode(&mut self, node: &Node, path: &str) -> Result {
        match node {
            Node::Null => {
                self.clear();
                Ok(())
            }
            Node::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    if i == self.len() {
                        self.push(T::default());
                    }
                    self[i].decode(item, &format!("{path}[{i}]"))?;
                }
                self.truncate(items.len());
                Ok(())
            }
            other => Err(mismatch(path, other, "an array")),
        }
    }
}

/// Declares a raw struct and its decoder from one list of fields.
macro_rules! raw_struct {
    ($(#[$meta:meta])* $name:ident, $what:literal { $($field:ident: $ty:ty),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Default)]
        pub(crate) struct $name {
            $(pub(crate) $field: $ty,)*
        }

        impl Decode for $name {
            fn decode(&mut self, node: &Node, path: &str) -> Result {
                members(node, path, $what, |name, value, at| match name {
                    $(stringify!($field) => Some(self.$field.decode(value, at)),)*
                    _ => None,
                })
            }
        }
    };
}

raw_struct!(RawConfig, "an object" {
    listen: String,
    admin_listen: String,
    trusted_proxies: Option<Vec<String>>,
    tls: Option<RawTls>,
    routes: Option<Vec<RawRoute>>,
    clusters: Option<Vec<RawCluster>>,
});

raw_struct!(RawTls, "an object" {
    certificates: Option<Vec<RawCertificate>>,
    min_version: String,
    cipher_policy: String,
    cipher_suites: Option<Vec<String>>,
    handshake_timeout: Option<Duration>,
    max_concurrent_handshakes: Option<i64>,
    strict_sni: bool,
});

raw_struct!(RawCertificate, "an object" {
    cert_file: String,
    key_file: String,
    server_names: Option<Vec<String>>,
});

raw_struct!(RawRoute, "an object" {
    host: String,
    path_prefix: String,
    cluster: String,
});

raw_struct!(RawCluster, "an object" {
    name: String,
    policy: String,
    hash_header: String,
    endpoints: Option<Vec<RawEndpoint>>,
    connect_timeout: Option<Duration>,
    timeout: Option<Duration>,
    max_requests: Option<i64>,
    panic_threshold_percent: Option<i64>,
    health_check: Option<RawHealth>,
    outlier_detection: Option<RawOutlier>,
    retry: Option<RawRetry>,
});

raw_struct!(RawEndpoint, "an object" {
    address: String,
    weight: Option<i64>,
    priority: Option<i64>,
});

raw_struct!(RawHealth, "an object" {
    path: String,
    host: String,
    interval: Option<Duration>,
    jitter: Option<Duration>,
    timeout: Option<Duration>,
    healthy_threshold: Option<i64>,
    unhealthy_threshold: Option<i64>,
});

raw_struct!(RawOutlier, "an object" {
    consecutive_gateway_failures: Option<i64>,
    interval: Option<Duration>,
    base_ejection_time: Option<Duration>,
    max_ejection_time: Option<Duration>,
    max_ejection_percent: Option<i64>,
    success_rate_minimum_hosts: Option<i64>,
    success_rate_request_volume: Option<i64>,
    success_rate_stdev_factor: Option<f64>,
});

raw_struct!(RawRetry, "an object" {
    max_retries: Option<i64>,
    retry_on: Option<Vec<i64>>,
    per_try_timeout: Option<Duration>,
    backoff_base: Option<Duration>,
    backoff_max: Option<Duration>,
    max_body_bytes: Option<i64>,
    budget: Option<RawBudget>,
});

raw_struct!(RawBudget, "an object" {
    ratio: Option<f64>,
    min_per_second: Option<i64>,
    window: Option<Duration>,
});

/// Decodes a whole document.
pub(crate) fn decode(node: &Node) -> Result<RawConfig> {
    let mut raw = RawConfig::default();
    raw.decode(node, "")?;
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::super::node::parse_json;
    use super::*;

    fn raw(json: &str) -> Result<RawConfig> {
        decode(&parse_json(json.as_bytes()).unwrap())
    }

    #[test]
    fn keys_fold_like_go() {
        let r = raw(r#"{"LISTEN":":1","Admin_Listen":":2","tls":{"ſtrict_ſni":true}}"#).unwrap();
        assert_eq!(r.listen, ":1");
        assert_eq!(r.admin_listen, ":2");
        assert!(r.tls.unwrap().strict_sni);
        let r = raw(r#"{"clusters":[{"bacKoff":1}]}"#);
        assert!(r.is_err());
        let r = raw(r#"{"clusters":[{"retry":{"bacKoff_base":"1s"}}]}"#).unwrap();
        let retry = r.clusters.unwrap().remove(0).retry.unwrap();
        assert_eq!(retry.backoff_base, Some(Duration(1_000_000_000)));
        // Only those two non-ASCII letters fold.
        let err = raw(r#"{"lısten":":1"}"#).unwrap_err();
        assert!(err.message.contains("unknown field"), "{}", err.message);
        let err = raw(r#"{"listen_":":1"}"#).unwrap_err();
        assert_eq!(err.path, "(top level)");
    }

    #[test]
    fn duplicate_keys_merge_like_go() {
        let r = raw(
            r#"{"clusters":[{"name":"a","retry":{"max_retries":9}},{"name":"b"}],
                "clusters":[{"retry":{"backoff_base":"1ms"}}]}"#,
        )
        .unwrap();
        let clusters = r.clusters.unwrap();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].name, "a");
        let retry = clusters[0].retry.as_ref().unwrap();
        assert_eq!(retry.max_retries, Some(9));
        assert_eq!(retry.backoff_base, Some(Duration(1_000_000)));
        let r = raw(r#"{"listen":":1","LISTEN":":2"}"#).unwrap();
        assert_eq!(r.listen, ":2");
    }

    #[test]
    fn null_clears_options_and_keeps_plain_values() {
        let r = raw(r#"{"listen":":1","listen":null,"tls":{},"tls":null,
                "clusters":[{"max_requests":5,"max_requests":null,"name":"a","name":null,
                             "retry":{"retry_on":[503],"retry_on":null}}],
                "trusted_proxies":["a"],"trusted_proxies":null}"#)
        .unwrap();
        assert_eq!(r.listen, ":1");
        assert!(r.tls.is_none());
        assert!(r.trusted_proxies.is_none());
        let c = &r.clusters.unwrap()[0];
        assert_eq!(c.max_requests, None);
        assert_eq!(c.name, "a");
        assert_eq!(c.retry.as_ref().unwrap().retry_on, None);
        // An empty list is not an absent one.
        let r = raw(r#"{"trusted_proxies":[]}"#).unwrap();
        assert_eq!(r.trusted_proxies, Some(vec![]));
        // A null element of a list of numbers keeps what was there.
        let r =
            raw(r#"{"clusters":[{"retry":{"retry_on":[502,503],"retry_on":[null,504,null]}}]}"#)
                .unwrap();
        let retry = r.clusters.unwrap().remove(0).retry.unwrap();
        assert_eq!(retry.retry_on, Some(vec![502, 504, 0]));
        // A top-level null decodes to nothing at all.
        assert!(raw("null").unwrap().clusters.is_none());
    }

    #[test]
    fn numbers_decode_like_go() {
        let max = |v: &str| {
            raw(&format!(r#"{{"clusters":[{{"max_requests":{v}}}]}}"#))
                .map(|r| r.clusters.unwrap()[0].max_requests)
        };
        assert_eq!(max("5").unwrap(), Some(5));
        assert_eq!(max("-0").unwrap(), Some(0));
        assert_eq!(max("9223372036854775807").unwrap(), Some(i64::MAX));
        for bad in [
            "5.0",
            "5e0",
            "9223372036854775808",
            "\"5\"",
            "true",
            "[]",
            "{}",
        ] {
            let err = max(bad).unwrap_err();
            assert_eq!(err.path, "clusters[0].max_requests", "{bad}");
        }
        let factor = |v: &str| {
            raw(&format!(
                r#"{{"clusters":[{{"outlier_detection":{{"success_rate_stdev_factor":{v}}}}}]}}"#
            ))
            .map(|r| {
                r.clusters.unwrap()[0]
                    .outlier_detection
                    .as_ref()
                    .unwrap()
                    .success_rate_stdev_factor
            })
        };
        assert_eq!(factor("1.5").unwrap(), Some(1.5));
        assert_eq!(factor("2").unwrap(), Some(2.0));
        assert_eq!(factor("1e-400").unwrap(), Some(0.0));
        assert!(
            factor("1e400")
                .unwrap_err()
                .message
                .contains("out of range")
        );
        assert!(factor("\"1\"").is_err());
    }

    #[test]
    fn durations_must_be_strings() {
        let t = |v: &str| {
            raw(&format!(r#"{{"clusters":[{{"timeout":{v}}}]}}"#))
                .map(|r| r.clusters.unwrap()[0].timeout)
        };
        assert_eq!(t("\"1.5s\"").unwrap(), Some(Duration(1_500_000_000)));
        assert_eq!(t("null").unwrap(), None);
        assert!(
            t("5")
                .unwrap_err()
                .message
                .contains("durations are strings")
        );
        assert!(
            t("{}")
                .unwrap_err()
                .message
                .contains("durations are strings")
        );
        let err = t("\"soon\"").unwrap_err();
        assert!(err.message.contains("invalid duration"));
        assert_eq!(err.path, "clusters[0].timeout");
    }

    #[test]
    fn type_errors_name_the_field() {
        for (doc, path) in [
            (r#"[]"#, "(top level)"),
            (r#"{"listen":1}"#, "listen"),
            (r#"{"tls":[]}"#, "tls"),
            (r#"{"tls":{"strict_sni":"yes"}}"#, "tls.strict_sni"),
            (r#"{"routes":{}}"#, "routes"),
            (r#"{"routes":[1]}"#, "routes[0]"),
            (
                r#"{"clusters":[{},{"endpoints":[{"weight":"1"}]}]}"#,
                "clusters[1].endpoints[0].weight",
            ),
            (
                r#"{"clusters":[{"retry":{"budget":{"window":1}}}]}"#,
                "clusters[0].retry.budget.window",
            ),
            (
                r#"{"tls":{"certificates":[{"x":1}]}}"#,
                "tls.certificates[0]",
            ),
        ] {
            let err = raw(doc).unwrap_err();
            assert_eq!(err.path, path, "{doc}: {}", err.message);
        }
    }
}
