use std::path::{Path, PathBuf};

use super::*;

const BASIC: &str = include_str!("../../../tests/data/envoy_ratelimit/basic_config.yaml");
const WILDCARD: &str = include_str!("../../../tests/data/envoy_ratelimit/wildcard.yaml");

/// A scratch directory removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "edgefront-rls-config-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn file(name: &str, text: &str) -> ConfigFile {
    ConfigFile {
        name: name.to_owned(),
        text: text.to_owned(),
    }
}

/// The findings as `(file, line, class)`.
fn found(files: &[(&str, &str)]) -> Vec<(String, usize, &'static str)> {
    lint(files)
        .unwrap()
        .1
        .into_iter()
        .map(|l| (l.file, l.line, l.class.as_str()))
        .collect()
}

#[test]
fn settings_default_as_the_service_does() {
    let s = RlsSettings::default();
    assert_eq!(s.runtime_root, Path::new("/srv/runtime_data/current"));
    assert_eq!(s.runtime_subdirectory, "");
    assert_eq!(s.runtime_app_directory, "config");
    assert!(!s.runtime_ignore_dotfiles && !s.merge_domain_config && !s.shadow_mode);
    assert_eq!(
        s.config_dir(),
        Path::new("/srv/runtime_data/current/config")
    );
    assert_eq!(RlsSettings::from_env(|_| None).unwrap(), s);
}

#[test]
fn settings_come_from_the_services_environment_variables() {
    let env = [
        ("RUNTIME_ROOT", "/data"),
        ("RUNTIME_SUBDIRECTORY", "ratelimit"),
        ("RUNTIME_APPDIRECTORY", "limits"),
        ("RUNTIME_IGNOREDOTFILES", "true"),
        ("MERGE_DOMAIN_CONFIG", "1"),
        ("SHADOW_MODE", "T"),
    ];
    let get = |k: &str| {
        env.iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| (*v).to_owned())
    };
    let s = RlsSettings::from_env(get).unwrap();
    assert_eq!(
        s,
        RlsSettings {
            runtime_root: PathBuf::from("/data"),
            runtime_subdirectory: "ratelimit".to_owned(),
            runtime_app_directory: "limits".to_owned(),
            runtime_ignore_dotfiles: true,
            merge_domain_config: true,
            shadow_mode: true,
        }
    );
    assert_eq!(s.config_dir(), Path::new("/data/ratelimit/limits"));
    for (text, want) in [
        ("0", false),
        ("f", false),
        ("F", false),
        ("FALSE", false),
        ("False", false),
        ("t", true),
        ("TRUE", true),
        ("True", true),
    ] {
        let s = RlsSettings::from_env(|k| (k == "SHADOW_MODE").then(|| text.to_owned())).unwrap();
        assert_eq!(s.shadow_mode, want, "{text}");
    }
    for bad in ["yes", "on", "", " true", "tRUE"] {
        let e = RlsSettings::from_env(|k| (k == "MERGE_DOMAIN_CONFIG").then(|| bad.to_owned()))
            .unwrap_err();
        assert_eq!(e, format!("MERGE_DOMAIN_CONFIG: invalid boolean '{bad}'"));
    }
}

#[test]
fn the_toml_section_reads_every_setting_and_refuses_unknown_ones() {
    let text = r#"
workers = 2

[rls]
runtime_root = "runtime"
runtime_subdirectory = "ratelimit"
shadow_mode = true
"#;
    let sec: RlsSection = toml::from_str(text).unwrap();
    let s = sec.rls.unwrap();
    assert!(s.shadow_mode && !s.merge_domain_config);
    assert_eq!(s.runtime_app_directory, "config");
    let s = s.relative_to(Path::new("/etc/edgefront"));
    assert_eq!(
        s.config_dir(),
        Path::new("/etc/edgefront/runtime/ratelimit/config")
    );
    // An absolute root stays where it is.
    let abs = RlsSettings::default().relative_to(Path::new("/etc"));
    assert_eq!(abs.runtime_root, Path::new(DEFAULT_RUNTIME_ROOT));
    let none: RlsSection = toml::from_str("workers = 1\n").unwrap();
    assert_eq!(none.rls, None);
    let e = toml::from_str::<RlsSection>("[rls]\nruntime_rot = \"x\"\n").unwrap_err();
    assert!(e.to_string().contains("unknown field `runtime_rot`"), "{e}");
}

#[test]
fn files_are_read_from_the_runtime_layout_in_path_order() {
    let dir = Scratch::new("layout");
    dir.write("ratelimit/config/b.yaml", "domain: b\n");
    dir.write("ratelimit/config/a.yaml", "domain: a\n");
    dir.write("ratelimit/config/team/c.yml", "domain: c\n");
    dir.write("ratelimit/config/d", "domain: d\n");
    dir.write("ratelimit/config/.e.yaml.swp", "domain: e\n");
    dir.write("ratelimit/config/.hidden/f.yaml", "domain: f\n");
    // Outside the app directory: not config.
    dir.write("ratelimit/other.yaml", "domain: other\n");
    dir.write("ratelimit/runtime/flag", "1\n");
    let mut s = RlsSettings {
        runtime_root: dir.path().to_owned(),
        runtime_subdirectory: "ratelimit".to_owned(),
        ..RlsSettings::default()
    };
    let names = |s: &RlsSettings| -> Vec<String> {
        s.read_files()
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect()
    };
    assert_eq!(
        names(&s),
        [
            "config/.e.yaml.swp",
            "config/.hidden/f.yaml",
            "config/a.yaml",
            "config/b.yaml",
            "config/d",
            "config/team/c.yml",
        ]
    );
    s.runtime_ignore_dotfiles = true;
    assert_eq!(
        names(&s),
        [
            "config/a.yaml",
            "config/b.yaml",
            "config/d",
            "config/team/c.yml"
        ]
    );
    let files = s.read_files().unwrap();
    assert_eq!(files[0].text, "domain: a\n");
    let loaded = load_settings(&s).unwrap();
    let mut domains: Vec<&str> = loaded.config.domains().collect();
    domains.sort_unstable();
    assert_eq!(domains, ["a", "b", "c", "d"]);
    assert_eq!(loaded.stats.files, 4);
    // With no app directory the whole subdirectory is config.
    s.runtime_app_directory = String::new();
    assert_eq!(names(&s).len(), 6);
    assert_eq!(names(&s)[0], "config/a.yaml");
    assert_eq!(names(&s)[4], "other.yaml");
}

#[cfg(unix)]
#[test]
fn symbolic_links_are_followed_as_a_release_swap_uses_them() {
    let dir = Scratch::new("symlink");
    dir.write("releases/1/config/api.yaml", "domain: v1\n");
    dir.write("releases/2/config/api.yaml", "domain: v2\n");
    dir.write("shared/extra.yaml", "domain: extra\n");
    let current = dir.path().join("current");
    std::os::unix::fs::symlink(dir.path().join("releases/1"), &current).unwrap();
    std::os::unix::fs::symlink(
        dir.path().join("shared/extra.yaml"),
        dir.path().join("releases/1/config/extra.yaml"),
    )
    .unwrap();
    let s = RlsSettings {
        runtime_root: current.clone(),
        ..RlsSettings::default()
    };
    let loaded = load_settings(&s).unwrap();
    assert_eq!(loaded.files, ["config/api.yaml", "config/extra.yaml"]);
    std::fs::remove_file(&current).unwrap();
    std::os::unix::fs::symlink(dir.path().join("releases/2"), &current).unwrap();
    let loaded = load_settings(&s).unwrap();
    assert_eq!(loaded.config.domains().collect::<Vec<_>>(), ["v2"]);
    // A loop is refused, not followed for ever.
    std::os::unix::fs::symlink(
        dir.path().join("releases/2/config"),
        dir.path().join("releases/2/config/loop"),
    )
    .unwrap();
    let e = load_settings(&s).unwrap_err();
    assert!(e.contains("directories deep, a symbolic link loop?"), "{e}");
}

#[test]
fn unreadable_layouts_are_errors() {
    let dir = Scratch::new("missing");
    let s = RlsSettings {
        runtime_root: dir.path().join("nowhere"),
        ..RlsSettings::default()
    };
    let e = s.read_files().unwrap_err();
    assert!(
        e.starts_with("cannot read ") && e.contains("nowhere/config"),
        "{e}"
    );
    dir.write("config/bin.yaml", "");
    std::fs::write(dir.path().join("config/bin.yaml"), [0xff, 0xfe, 0]).unwrap();
    let s = RlsSettings {
        runtime_root: dir.path().to_owned(),
        ..RlsSettings::default()
    };
    assert_eq!(
        s.read_files().unwrap_err(),
        "config/bin.yaml: error loading config file: not UTF-8"
    );
    // An empty directory is a deployment with no domains, as the service
    // runs one.
    std::fs::remove_file(dir.path().join("config/bin.yaml")).unwrap();
    let loaded = load_settings(&s).unwrap();
    assert_eq!(loaded.stats, RlsStats::default());
    assert!(loaded.lints.is_empty());
}

#[test]
fn the_services_own_configs_load_unchanged_and_are_clean() {
    let loaded = load(&[file("config/basic_config.yaml", BASIC)], false).unwrap();
    assert_eq!(
        loaded.stats,
        RlsStats {
            files: 1,
            domains: 1,
            descriptors: 13,
            limits: 10,
            unlimited: 1,
            shadow: 0,
        }
    );
    assert_eq!(
        loaded.stats.to_string(),
        "1 file, 1 domain, 13 descriptors, 10 limits, 1 unlimited, 0 in shadow mode"
    );
    assert!(loaded.lints.is_empty(), "{:?}", loaded.lints);
    let l = loaded
        .config
        .get_limit("test-domain", &[("key1", "value1"), ("subkey1", "x")])
        .unwrap();
    assert_eq!(l.requests_per_unit, 5);
    let wild = load(&[file("config/wildcard.yaml", WILDCARD)], false).unwrap();
    assert!(wild.lints.is_empty(), "{:?}", wild.lints);
    assert_eq!(wild.stats.descriptors, 7);
    // Both together share a domain: refused, naming the later file, unless
    // merged.
    let both = [
        file("config/basic_config.yaml", BASIC),
        file("config/wildcard.yaml", WILDCARD),
    ];
    assert_eq!(
        load(&both, false).unwrap_err(),
        "config/wildcard.yaml: duplicate domain 'test-domain' in config file"
    );
    let merged = load(&both, true).unwrap();
    assert_eq!(merged.stats.domains, 1);
    assert_eq!(merged.stats.descriptors, 20);
    assert_eq!(
        merged.files,
        ["config/basic_config.yaml", "config/wildcard.yaml"]
    );
}

#[test]
fn shadow_mode_replaces_and_unlimited_load_as_the_service_reads_them() {
    let yaml = "\
domain: api
descriptors:
  - key: path
    value: /login
    shadow_mode: true
    rate_limit: {unit: minute, requests_per_unit: 5, name: login}
    descriptors:
      - key: user
        rate_limit:
          unit: minute
          requests_per_unit: 1
          replaces:
            - name: login
  - key: tenant
    value: internal
    rate_limit:
      unlimited: true
";
    let loaded = load(&[file("config/api.yaml", yaml)], false).unwrap();
    assert!(loaded.lints.is_empty(), "{:?}", loaded.lints);
    assert_eq!(
        (
            loaded.stats.limits,
            loaded.stats.unlimited,
            loaded.stats.shadow
        ),
        (2, 1, 1)
    );
    let cfg = &loaded.config;
    let login = cfg.get_limit("api", &[("path", "/login")]).unwrap();
    assert!(login.shadow_mode && login.name.as_deref() == Some("login"));
    let user = cfg
        .get_limit("api", &[("path", "/login"), ("user", "u")])
        .unwrap();
    assert_eq!(user.replaces, ["login"]);
    assert!(!user.shadow_mode);
    let tenant = cfg.get_limit("api", &[("tenant", "internal")]).unwrap();
    assert!(tenant.unlimited && tenant.unit.is_none());
    // The service's refusals come through with the file's name.
    let bad =
        "domain: api\ndescriptors:\n  - key: k\n    rate_limit: {unlimited: true, unit: day}\n";
    assert_eq!(
        load(&[file("config/bad.yaml", bad)], false).unwrap_err(),
        "config/bad.yaml: should not specify rate limit unit when unlimited"
    );
}

#[test]
fn zero_limits_are_found_unless_in_shadow_mode() {
    let yaml = "\
domain: d
descriptors:
  - key: a
    rate_limit:
      unit: second
  - key: b
    rate_limit:
      unit: hour
      requests_per_unit: 0
  - key: c
    shadow_mode: true
    rate_limit: {unit: second, requests_per_unit: 0}
  - key: e
    rate_limit: {unit: second, requests_per_unit: 1}
";
    assert_eq!(
        found(&[("z.yaml", yaml)]),
        [
            ("z.yaml".to_owned(), 5, "rls-zero-limit"),
            ("z.yaml".to_owned(), 9, "rls-zero-limit"),
        ]
    );
    let (_, lints) = lint(&[("z.yaml", yaml)]).unwrap();
    assert_eq!(
        lints[0].to_string(),
        "warning[rls-zero-limit]: descriptor d.a: the limit allows 0 requests per second (requests_per_unit is missing), so every request it matches is over the limit"
    );
    assert_eq!(
        lints[1].to_string(),
        "warning[rls-zero-limit]: descriptor d.b: the limit allows 0 requests per hour, so every request it matches is over the limit"
    );
}

#[test]
fn replaces_must_name_a_limit_of_the_domain() {
    let a = "\
domain: d
descriptors:
  - key: a
    rate_limit:
      unit: second
      requests_per_unit: 1
      name: first
      replaces:
        - name: second
        - name: nowhere
";
    // `second` lives in another file of the same domain, merged; `elsewhere`
    // only in another domain.
    let b = "\
domain: d
descriptors:
  - key: b
    descriptors:
      - key: c
        rate_limit: {unit: second, requests_per_unit: 1, name: second, replaces: [{name: elsewhere}]}
";
    let c = "domain: other\ndescriptors:\n  - key: x\n    rate_limit: {unit: second, requests_per_unit: 1, name: elsewhere}\n";
    assert_eq!(
        found(&[("a.yaml", a), ("b.yaml", b), ("c.yaml", c)]),
        [
            ("a.yaml".to_owned(), 10, "rls-replaces-unknown"),
            ("b.yaml".to_owned(), 6, "rls-replaces-unknown"),
        ]
    );
    let (_, lints) = lint(&[("a.yaml", a), ("b.yaml", b), ("c.yaml", c)]).unwrap();
    assert_eq!(
        lints[1].to_string(),
        "warning[rls-replaces-unknown]: descriptor d.b.c: replaces 'elsewhere', but no limit in domain d has that name, so it replaces nothing"
    );
}

#[test]
fn shadow_mode_without_a_limit_is_found() {
    let yaml = "\
domain: d
descriptors:
  - key: a
    shadow_mode: true
  - key: b
    shadow_mode: true
    rate_limit: {unlimited: true}
  - key: c
    shadow_mode: false
";
    assert_eq!(
        found(&[("s.yaml", yaml)]),
        [
            ("s.yaml".to_owned(), 4, "rls-shadow-no-limit"),
            ("s.yaml".to_owned(), 6, "rls-shadow-no-limit"),
        ]
    );
    let (stats, lints) = lint(&[("s.yaml", yaml)]).unwrap();
    assert_eq!((stats.shadow, stats.unlimited, stats.limits), (2, 1, 0));
    assert!(lints[1].message.contains("is unlimited"));
    assert!(lints[0].message.contains("has no rate_limit"));
}

#[test]
fn wildcards_that_take_every_value_first_are_found() {
    let yaml = "\
domain: d
descriptors:
  - key: ua
    value: '*bot*'
    rate_limit: {unit: second, requests_per_unit: 1}
  - key: ua
    value: 'googlebot*'
    rate_limit: {unit: second, requests_per_unit: 5}
  - key: ua
    value: 'bot*'
    rate_limit: {unit: second, requests_per_unit: 5}
  - key: ua
    value: '*bo'
    rate_limit: {unit: second, requests_per_unit: 5}
  - key: ua
    value: exactbot
    rate_limit: {unit: second, requests_per_unit: 5}
  - key: other
    value: 'a*'
    rate_limit: {unit: second, requests_per_unit: 5}
  - key: other
    value: '*'
    rate_limit: {unit: second, requests_per_unit: 5}
  - key: other
    value: 'ab*'
    rate_limit: {unit: second, requests_per_unit: 5}
  - key: other
    rate_limit: {unit: second, requests_per_unit: 5}
";
    // googlebot* and bot* sit under *bot*; *bo does not ("xbo" has no
    // "bot"), and the exact value is tried before any wildcard. Under
    // `other`, ab* sits under a* and under *, and the key alone is never
    // reached past *.
    assert_eq!(
        found(&[("w.yaml", yaml)]),
        [
            ("w.yaml".to_owned(), 6, "rls-wildcard-shadowed"),
            ("w.yaml".to_owned(), 9, "rls-wildcard-shadowed"),
            ("w.yaml".to_owned(), 24, "rls-wildcard-shadowed"),
            ("w.yaml".to_owned(), 27, "rls-wildcard-shadowed"),
        ]
    );
    let (_, lints) = lint(&[("w.yaml", yaml)]).unwrap();
    assert_eq!(
        lints[0].to_string(),
        "warning[rls-wildcard-shadowed]: descriptor d.ua_googlebot*: never reached: the wildcard value '*bot*' on line 3 matches every value this descriptor would, and is tried first"
    );
    assert!(lints[2].message.contains("'a*' on line 18"));
    assert!(lints[3].message.contains("'*' on line 21"));
    // The decision agrees: googlebot gets the *bot* limit, and `other`
    // with any value never gets the key's own limit.
    let cfg = DescriptorConfig::load(&[("w.yaml", yaml)], false).unwrap();
    let full = |entries: &[(&str, &str)]| cfg.get_limit("d", entries).unwrap().full_key;
    assert_eq!(full(&[("ua", "googlebot-2")]), "d.ua_*bot*");
    assert_eq!(full(&[("other", "zz")]), "d.other_*");
    assert_eq!(full(&[("other", "")]), "d.other_*");
}

#[test]
fn a_wildcard_in_another_file_is_named_with_its_file() {
    let a = "domain: d\ndescriptors:\n  - key: k\n    value: '*'\n    rate_limit: {unit: second, requests_per_unit: 1}\n";
    let b = "domain: d\ndescriptors:\n  - key: k\n    value: 'x*'\n    rate_limit: {unit: second, requests_per_unit: 1}\n";
    let (_, lints) = lint(&[("a.yaml", a), ("b.yaml", b)]).unwrap();
    assert_eq!(lints.len(), 1);
    assert_eq!((lints[0].file.as_str(), lints[0].line), ("b.yaml", 3));
    assert!(
        lints[0].message.contains("'*' on a.yaml line 3"),
        "{}",
        lints[0].message
    );
}

#[test]
fn every_rls_class_has_a_name_that_parses_back() {
    for c in [
        Class::RlsZeroLimit,
        Class::RlsReplacesUnknown,
        Class::RlsShadowNoLimit,
        Class::RlsWildcardShadowed,
    ] {
        assert!(c.as_str().starts_with("rls-"));
        assert_eq!(Class::from_name(c.as_str()), Some(c));
    }
}
