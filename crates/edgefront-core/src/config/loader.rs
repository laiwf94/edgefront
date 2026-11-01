//! Loading TOML config files and the secrets that stay out of them, for
//! products whose config is not steer's schema.
//!
//! Each product describes its file as serde structs with
//! `#[serde(deny_unknown_fields)]` and implements [`Validate`] for the
//! checks serde cannot express; [`parse`] runs both. Secrets come from the
//! environment through [`env_secret`] and print as `<set>` through
//! [`Redacted`], so a config can be committed and logged.

use std::fmt;

use http::Uri;
use http::uri::Authority;
use serde::de::DeserializeOwned;

/// Checks a parsed config for what its types cannot.
pub trait Validate {
    /// Returns a message naming the offending field when the config is
    /// unusable.
    fn validate(&self) -> Result<(), String>;
}

/// Parses `text` as TOML into `T` and validates it. The error is a message
/// for a person: the TOML parser's, with line and column, or the
/// validator's.
pub fn parse<T: DeserializeOwned + Validate>(text: &str) -> Result<T, String> {
    let config: T = toml::from_str(text).map_err(|e| e.to_string())?;
    config.validate()?;
    Ok(config)
}

/// Reads the environment variable `name`, treating an empty value as
/// unset.
pub fn env_secret(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Debug-prints an optional secret as `Some("<set>")` or `None`, never its
/// value.
pub struct Redacted<'a, T>(pub &'a Option<T>);

impl<T> fmt::Debug for Redacted<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0.as_ref().map(|_| "<set>"), f)
    }
}

/// Parses `http://host:port` with nothing after the authority.
pub fn parse_base_url(url: &str) -> Result<Authority, String> {
    let uri: Uri = url
        .parse()
        .map_err(|e| format!("{url:?} is not a URL: {e}"))?;
    if uri.scheme_str() != Some("http") {
        return Err(format!("{url:?} must use http://"));
    }
    if uri.path_and_query().is_some_and(|p| p.as_str() != "/") {
        return Err(format!("{url:?} must not have a path"));
    }
    let authority = uri
        .authority()
        .ok_or_else(|| format!("{url:?} has no host"))?
        .clone();
    if authority.as_str().contains('@') {
        return Err(format!("{url:?} must not contain credentials"));
    }
    Ok(authority)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct File {
        name: String,
        #[serde(default)]
        port: u16,
    }

    impl Validate for File {
        fn validate(&self) -> Result<(), String> {
            if self.port == 0 {
                return Err("port must be positive".into());
            }
            Ok(())
        }
    }

    #[test]
    fn parse_runs_serde_then_the_validator() {
        let f: File = parse("name = \"a\"\nport = 80").unwrap();
        assert_eq!((f.name.as_str(), f.port), ("a", 80));
        let err = parse::<File>("name = \"a\"").unwrap_err();
        assert_eq!(err, "port must be positive");
        let err = parse::<File>("name = \"a\"\nport = 1\nother = 2").unwrap_err();
        assert!(err.contains("unknown field `other`"), "{err}");
        let err = parse::<File>("name = ").unwrap_err();
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn base_urls() {
        assert_eq!(parse_base_url("http://a:1").unwrap().as_str(), "a:1");
        assert_eq!(parse_base_url("http://a:1/").unwrap().as_str(), "a:1");
        assert_eq!(parse_base_url("http://a").unwrap().as_str(), "a");
        let cases = [
            ("https://a", "must use http://"),
            ("http://a/path", "must not have a path"),
            ("http://a/?q", "must not have a path"),
            ("a:1", "must use http://"),
            ("/path", "must use http://"),
            ("http://u:p@a", "must not contain credentials"),
            ("ht tp://", "is not a URL"),
        ];
        for (bad, want) in cases {
            let err = parse_base_url(bad).unwrap_err();
            assert!(err.contains(want), "{bad}: {err}");
        }
    }

    #[test]
    fn env_secrets() {
        // Edition 2024 makes set_var unsafe and the workspace forbids
        // unsafe code, so the test reads what the process inherited.
        assert_eq!(env_secret("EDGEFRONT_CORE_TEST_UNSET_SECRET"), None);
        assert_eq!(env_secret(""), None);
        if let Ok(path) = std::env::var("PATH")
            && !path.is_empty()
        {
            assert_eq!(env_secret("PATH"), Some(path));
        }
    }

    /// An empty variable counts as unset. The test reruns itself as a
    /// child process with the variables set, since this process cannot
    /// set them.
    #[test]
    fn empty_env_secret_is_unset() {
        if std::env::var_os("EDGEFRONT_CORE_TEST_CHILD").is_some() {
            assert_eq!(env_secret("EDGEFRONT_CORE_TEST_EMPTY"), None);
            assert_eq!(env_secret("EDGEFRONT_CORE_TEST_FULL").as_deref(), Some("x"));
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "config::loader::tests::empty_env_secret_is_unset",
                "--exact",
            ])
            .env("EDGEFRONT_CORE_TEST_CHILD", "1")
            .env("EDGEFRONT_CORE_TEST_EMPTY", "")
            .env("EDGEFRONT_CORE_TEST_FULL", "x")
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn redacted_secrets() {
        let set = Some("hunter2".to_string());
        assert_eq!(format!("{:?}", Redacted(&set)), "Some(\"<set>\")");
        assert_eq!(format!("{:?}", Redacted::<String>(&None)), "None");
    }
}
