//! `edgefront check` lints limiter configs in pacerd.conf and TOML syntax,
//! naming the file, line and class of every finding.

use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_edgefront");

fn edgefront(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().unwrap()
}

/// A scratch directory removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir =
            std::env::temp_dir().join(format!("edgefront-check-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn write(&self, name: &str, text: &str) -> String {
        let path = self.0.join(name);
        std::fs::write(&path, text).unwrap();
        path.to_str().unwrap().to_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Every lint class at least once, with the line each is expected on.
const PACERD: &str = "\
listener all {
    listen 0.0.0.0:80
    upstream 127.0.0.1:80
    max_conns 100
    per_ip_max_conns 100
    table_size 64
    bandwidth 16g/s burst 1k
    per_ip_bandwidth 32g/s burst 2k
}
listener one {
    listen 10.0.0.1:80
    upstream 10.0.0.9:80
    table_full evict
    ipv6_prefix 56
}
listener six {
    listen [::]:443
    upstream [2001:db8::1]:443
    per_ip_conn_rate 10/s
    ipv6_prefix 32
}
";

/// The same config as the limiter section of an edgefront config, split
/// so the third listener's lines are easy to count.
const TOML: &str = r#"workers = 1

[[listeners]]
name = "all"
listen = "0.0.0.0:80"
upstream = "127.0.0.1:80"
max_conns = 100
per_ip_max_conns = 100
table_size = 64
bandwidth = "16g/s burst 1k"
per_ip_bandwidth = "32g/s burst 2k"

[[listeners]]
name = "one"
listen = "10.0.0.1:80"
upstream = "10.0.0.9:80"
table_full = "evict"
ipv6_prefix = 56
"#;

const TOML_SIX: &str = r#"
[[listeners]]
name = "six"
listen = "[::]:443"
upstream = "[2001:db8::1]:443"
per_ip_conn_rate = "10/s"
ipv6_prefix = 32
per_ip_conn_quota = "864010/day"
"#;

/// `(line, class)` for each finding line of `stdout`, which must all name
/// `path`, and the summary line.
fn findings(stdout: &[u8], path: &str) -> (Vec<(usize, String)>, String) {
    let text = String::from_utf8(stdout.to_vec()).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    let summary = lines.pop().unwrap().to_owned();
    let found = lines
        .iter()
        .map(|l| {
            let rest = l
                .strip_prefix(&format!("{path}:"))
                .unwrap_or_else(|| panic!("{l}"));
            let (line, rest) = rest.split_once(": warning[").unwrap();
            let (class, _) = rest.split_once("]: listener ").unwrap();
            (line.parse().unwrap(), class.to_owned())
        })
        .collect();
    (found, summary)
}

fn expected(lines: &[(usize, &str)]) -> Vec<(usize, String)> {
    lines.iter().map(|&(l, c)| (l, c.to_owned())).collect()
}

#[test]
fn reports_every_class_with_its_line_in_pacerd_syntax() {
    let dir = Scratch::new("pacerd");
    let path = dir.write("pacerd.conf", PACERD);
    let out = edgefront(&["check", &path]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stderr.is_empty());
    let (found, summary) = findings(&out.stdout, &path);
    assert_eq!(
        found,
        expected(&[
            (3, "upstream-loop"),
            (5, "per-ip-cap-unreachable"),
            (6, "table-below-max-conns"),
            (7, "burst-below-read"),
            (7, "rate-rounding"),
            (8, "burst-below-read"),
            (8, "per-ip-bandwidth-unreachable"),
            (8, "rate-rounding"),
            (11, "listen-overlap"),
            (13, "unused-table"),
            (14, "ipv6-prefix-unused"),
            (20, "wide-ipv6-prefix"),
        ])
    );
    assert_eq!(summary, format!("{path}: 12 warnings"));
    let classes: std::collections::BTreeSet<_> = found.iter().map(|(_, c)| c.as_str()).collect();
    assert_eq!(classes.len(), 10, "{classes:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains(&format!(
        "{path}:6: warning[table-below-max-conns]: listener all: table_size 64 is below max_conns 100"
    )));
}

#[test]
fn reports_toml_lines_from_its_spans() {
    let dir = Scratch::new("toml");
    let path = dir.write("limiter.toml", &format!("{TOML}{TOML_SIX}"));
    let out = edgefront(&["check", &path]);
    assert_eq!(out.status.code(), Some(1));
    let (found, summary) = findings(&out.stdout, &path);
    assert_eq!(
        found,
        expected(&[
            (6, "upstream-loop"),
            (8, "per-ip-cap-unreachable"),
            (9, "table-below-max-conns"),
            (10, "burst-below-read"),
            (10, "rate-rounding"),
            (11, "burst-below-read"),
            (11, "per-ip-bandwidth-unreachable"),
            (11, "rate-rounding"),
            (15, "listen-overlap"),
            (17, "unused-table"),
            (18, "ipv6-prefix-unused"),
            (25, "wide-ipv6-prefix"),
            (26, "quota-unreachable"),
        ])
    );
    assert_eq!(summary, format!("{path}: 13 warnings"));
    // pacerd.conf has no quotas, so only TOML can show the last class.
    let classes: std::collections::BTreeSet<_> = found.iter().map(|(_, c)| c.as_str()).collect();
    assert_eq!(classes.len(), 11, "{classes:?}");

    // A default is reported on the line that opens its listener: the
    // `[[listeners]]` header, or an inline table's brace.
    let path = dir.write(
        "defaults.toml",
        "\n[[listeners]]\nname = \"a\"\nlisten = \"0.0.0.0:80\"\nupstream = \"10.0.0.9:80\"\nmax_conns = 20000\nper_ip_max_conns = 4\n",
    );
    let (found, _) = findings(&edgefront(&["check", &path]).stdout, &path);
    assert_eq!(found, expected(&[(2, "table-below-max-conns")]));
    let path = dir.write(
        "inline.toml",
        "listeners = [\n  { name = \"a\", listen = \"0.0.0.0:80\", upstream = \"10.0.0.9:80\", max_conns = 20000, per_ip_max_conns = 4 },\n]\n",
    );
    let (found, _) = findings(&edgefront(&["check", &path]).stdout, &path);
    assert_eq!(found, expected(&[(2, "table-below-max-conns")]));
}

#[test]
fn allow_drops_a_class_and_a_clean_config_is_ok() {
    let dir = Scratch::new("allow");
    let path = dir.write("pacerd.conf", PACERD);
    let mut args = vec!["check"];
    for c in [
        "upstream-loop",
        "per-ip-cap-unreachable",
        "table-below-max-conns",
        "burst-below-read",
        "rate-rounding",
        "per-ip-bandwidth-unreachable",
        "listen-overlap",
        "unused-table",
        "ipv6-prefix-unused",
    ] {
        args.extend(["--allow", c]);
    }
    args.push(&path);
    let out = edgefront(&args);
    assert_eq!(out.status.code(), Some(1));
    let (found, summary) = findings(&out.stdout, &path);
    assert_eq!(found, expected(&[(20, "wide-ipv6-prefix")]));
    assert_eq!(summary, format!("{path}: 1 warning"));

    args.insert(args.len() - 1, "--allow");
    args.insert(args.len() - 1, "wide-ipv6-prefix");
    let out = edgefront(&args);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{path}: ok\n")
    );

    let example = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/limiter/pacerd.conf"
    );
    let out = edgefront(&["check", example]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{example}: ok\n")
    );
}

#[test]
fn invalid_configs_and_bad_usage_exit_2() {
    let dir = Scratch::new("invalid");
    let path = dir.write(
        "bad.conf",
        "listener a {\n  listen 0.0.0.0:80\n  max_conns 0\n}\n",
    );
    let out = edgefront(&["check", &path]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        format!("edgefront: {path}: line 3: max_conns: must be between 1 and 1000000\n")
    );

    let path = dir.write("bad.toml", "[[listeners]]\nname = \"a\"\nmax_conns = 0\n");
    let out = edgefront(&["check", &path]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.starts_with(&format!("edgefront: {path}: ")), "{err}");
    assert!(
        err.contains("listeners[0].max_conns: must be between 1 and 1000000"),
        "{err}"
    );

    let missing = dir.0.join("missing.conf");
    let out = edgefront(&["check", missing.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("missing.conf"), "{err}");
    let out = edgefront(&["check", dir.0.join("missing.toml").to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .starts_with("edgefront: cannot read ")
    );

    let usage =
        "usage: edgefront check [--allow CLASS]... <file>\n       edgefront check --print <file>\n";
    for args in [
        &["check"][..],
        &["check", "--print"],
        &["check", "--print", "--allow"],
        &["check", "--print", "a", "b"],
        &["check", "a", "b"],
        &["check", "--allow"],
        &["check", "--allow", "rate-rounding"],
        &["check", "--bogus"],
    ] {
        let out = edgefront(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert_eq!(String::from_utf8(out.stderr).unwrap(), usage, "{args:?}");
    }
    let out = edgefront(&["check", "--allow", "nope", &path]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.starts_with(
            "edgefront: unknown lint class \"nope\"; the classes are burst-below-read, "
        ),
        "{err}"
    );
}

#[test]
fn print_writes_the_config_as_toml_that_reads_back_the_same() {
    let dir = Scratch::new("print");
    let example = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/limiter/pacerd.conf"
    );
    let out = edgefront(&["check", "--print", example]);
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty());
    let toml = String::from_utf8(out.stdout).unwrap();
    assert!(toml.contains("[[listeners]]\nname = \"web\"\n"), "{toml}");
    assert!(toml.contains("bandwidth = \"100m/s burst 1m\"\n"), "{toml}");
    let converted = dir.write("converted.toml", &toml);
    let again = edgefront(&["check", "--print", &converted]);
    assert!(again.status.success());
    assert_eq!(String::from_utf8(again.stdout).unwrap(), toml);
    let out = edgefront(&["check", &converted]);
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{converted}: ok\n")
    );

    // A TOML file comes back in canonical form: rates in lowest terms,
    // quotas in their whole windows, and defaults left out.
    let path = dir.write(
        "quota.toml",
        "[[listeners]]\nname = \"api\"\nlisten = \"0.0.0.0:8443\"\nupstream = \"10.0.0.2:8443\"\n\
         per_ip_conn_rate = \"0.5/s burst 10\"\nper_ip_conn_quota = \"1.5/h\"\n",
    );
    let out = edgefront(&["check", "--print", &path]);
    assert!(out.status.success());
    let toml = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        toml,
        "[[listeners]]\nname = \"api\"\nlisten = \"0.0.0.0:8443\"\nupstream = \"10.0.0.2:8443\"\n\
         per_ip_conn_rate = \"1/2s burst 10\"\nper_ip_conn_quota = \"3/2h\"\n"
    );

    // Errors are those of a plain check.
    let bad = dir.write("bad.toml", "[[listeners]]\nname = \"a\"\n");
    let out = edgefront(&["check", "--print", &bad]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("listeners[0]: listener a: missing listen")
    );
    // pacerd takes admin paths that are not UTF-8, and TOML cannot hold one.
    let mut conf = b"admin_socket /run/\xff.sock\n".to_vec();
    conf.extend_from_slice(b"listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n}\n");
    let path = dir.0.join("latin1.conf");
    std::fs::write(&path, conf).unwrap();
    let out = edgefront(&["check", "--print", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("admin_socket: the path is not UTF-8")
    );
}

const HTTP_RULES: &str = "\
[[http_limits]]
name = \"login\"
rate = \"5/min burst 5\"
key = [\"header:x-user\"]
count = [401, 403]
[http_limits.match]
path = \"/login\"

[[http_limits]]
name = \"never\"
rate = \"1/s\"
key = [\"path_prefix:/admin\", \"header:host\"]
max_delay = \"1s\"
[http_limits.match]
path_prefix = \"/api\"
";

#[test]
fn http_rules_are_built_and_linted() {
    let dir = Scratch::new("http");
    let path = dir.write("limits.toml", HTTP_RULES);
    let out = edgefront(&["check", &path]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert_eq!(
        stdout,
        format!(
            "http_limits: 2 rules\n\
             {path}:9: warning[http-key-never-matches]: http limit never: key path_prefix:/admin lies outside match.path_prefix /api, so no request has the key and the rule never applies\n\
             {path}:9: warning[http-key-host-header]: http limit never: key header:host finds nothing in HTTP/2 requests, which carry :authority; the host selector reads both\n\
             {path}:9: warning[http-delay-unused]: http limit never: max_delay is set but the action is block, so it never delays\n\
             {path}: 3 warnings\n"
        )
    );
    let out = edgefront(&[
        "check",
        "--allow",
        "http-key-never-matches",
        "--allow",
        "http-key-host-header",
        "--allow",
        "http-delay-unused",
        &path,
    ]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("http_limits: 2 rules\n{path}: ok\n")
    );

    // Beside a limiter section, both are checked and the findings sorted
    // by line.
    let both = format!(
        "[[listeners]]\nname = \"a\"\nlisten = \"0.0.0.0:80\"\nupstream = \"10.0.0.1:80\"\nmax_conns = 100\nper_ip_max_conns = 100\n{HTTP_RULES}"
    );
    let path = dir.write("both.toml", &both);
    let stdout = String::from_utf8(edgefront(&["check", &path]).stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "http_limits: 2 rules");
    assert!(
        lines[1].contains(":6: warning[per-ip-cap-unreachable]"),
        "{stdout}"
    );
    assert!(
        lines[2].contains(":15: warning[http-key-never-matches]"),
        "{stdout}"
    );
    assert_eq!(*lines.last().unwrap(), format!("{path}: 4 warnings"));

    // A rule that does not build is a config that does not parse.
    let bad = dir.write(
        "bad.toml",
        "[[http_limits]]\nname = \"x\"\nrate = \"1/s\"\nkey = [\"ip\"]\n",
    );
    let out = edgefront(&["check", &bad]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("http_limits[0].key[0]: unknown key selector \"ip\"")
    );
}

/// An envoyproxy/ratelimit config with one finding of each `rls-*` class.
const RLS_API: &str = "\
domain: api
descriptors:
  - key: path
    value: /login
    rate_limit: {unit: minute, requests_per_unit: 10, name: login}
    descriptors:
      - key: user
        rate_limit: {unit: minute, requests_per_unit: 2, replaces: [{name: logon}]}
  - key: client
    rate_limit: {unit: second}
  - key: tenant
    shadow_mode: true
  - key: ua
    value: '*'
    rate_limit: {unit: second, requests_per_unit: 50}
  - key: ua
    value: 'curl*'
    rate_limit: {unit: second, requests_per_unit: 5}
";

#[test]
fn envoy_ratelimit_files_and_directories_are_loaded_and_linted() {
    let dir = Scratch::new("rls");
    let path = dir.write("api.yaml", RLS_API);
    let out = edgefront(&["check", &path]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert_eq!(
        stdout,
        format!(
            "rls: 1 file, 1 domain, 6 descriptors, 5 limits, 0 unlimited, 1 in shadow mode\n\
             {path}:8: warning[rls-replaces-unknown]: descriptor api.path_/login.user: replaces 'logon', but no limit in domain api has that name, so it replaces nothing\n\
             {path}:10: warning[rls-zero-limit]: descriptor api.client: the limit allows 0 requests per second (requests_per_unit is missing), so every request it matches is over the limit\n\
             {path}:12: warning[rls-shadow-no-limit]: descriptor api.tenant: shadow_mode is set but the descriptor has no rate_limit, so there is nothing to shadow\n\
             {path}:16: warning[rls-wildcard-shadowed]: descriptor api.ua_curl*: never reached: the wildcard value '*' on line 13 matches every value this descriptor would, and is tried first\n\
             {path}: 4 warnings\n"
        )
    );
    let mut args = vec!["check"];
    for c in [
        "rls-replaces-unknown",
        "rls-zero-limit",
        "rls-shadow-no-limit",
        "rls-wildcard-shadowed",
    ] {
        args.extend(["--allow", c]);
    }
    args.push(&path);
    let out = edgefront(&args);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .ends_with(&format!("{path}: ok\n"))
    );

    // The service's own test configs load unchanged and are clean.
    let data = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../edgefront-limiter/tests/data/envoy_ratelimit"
    );
    let basic = format!("{data}/basic_config.yaml");
    let out = edgefront(&["check", &basic]);
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!(
            "rls: 1 file, 1 domain, 13 descriptors, 10 limits, 1 unlimited, 0 in shadow mode\n{basic}: ok\n"
        )
    );
    assert_eq!(out.status.code(), Some(0));

    // A directory is the service's config directory: every file under it,
    // each finding on its own file. Two files with one domain are refused
    // as the service refuses them, naming the later file.
    let conf = dir.0.join("config");
    std::fs::create_dir_all(conf.join("team")).unwrap();
    std::fs::write(
        conf.join("a.yaml"),
        "domain: a\ndescriptors:\n  - key: k\n    rate_limit: {unit: hour, requests_per_unit: 0}\n",
    )
    .unwrap();
    std::fs::write(conf.join("team/b.yml"), "domain: b\n").unwrap();
    let conf_s = conf.to_str().unwrap();
    let out = edgefront(&["check", conf_s]);
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!(
            "rls: 2 files, 2 domains, 1 descriptor, 1 limit, 0 unlimited, 0 in shadow mode\n\
             {}:4: warning[rls-zero-limit]: descriptor a.k: the limit allows 0 requests per hour, so every request it matches is over the limit\n\
             {conf_s}: 1 warning\n",
            conf.join("a.yaml").display()
        )
    );
    assert_eq!(out.status.code(), Some(1));
    std::fs::write(conf.join("team/c.yaml"), "domain: a\n").unwrap();
    let out = edgefront(&["check", conf_s]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        format!(
            "edgefront: {}: duplicate domain 'a' in config file\n",
            conf.join("team/c.yaml").display()
        )
    );

    // The service's refusals are configs that do not parse.
    let bad = dir.write(
        "bad.yml",
        "domain: x\ndescriptors:\n  - key: k\n    ratelimit: {}\n",
    );
    let out = edgefront(&["check", &bad]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        format!("edgefront: {bad}: config error, unknown key 'ratelimit'\n")
    );
}

#[test]
fn an_rls_section_checks_the_deployment_it_points_at() {
    let dir = Scratch::new("rls-toml");
    let conf = dir.0.join("runtime/ratelimit/config");
    std::fs::create_dir_all(&conf).unwrap();
    std::fs::write(conf.join("api.yaml"), RLS_API).unwrap();
    std::fs::write(conf.join(".api.yaml.swp"), "not: [yaml").unwrap();
    std::fs::write(
        conf.join("more.yaml"),
        "domain: api\ndescriptors:\n  - key: logon\n    rate_limit: {unit: minute, requests_per_unit: 1, name: logon}\n",
    )
    .unwrap();
    // Relative to the TOML file; the swap file is a dot file.
    let toml = "[rls]\nruntime_root = \"runtime\"\nruntime_subdirectory = \"ratelimit\"\nruntime_ignore_dotfiles = true\nmerge_domain_config = true\nshadow_mode = true\n";
    let path = dir.write("edge.toml", toml);
    let out = edgefront(&["check", &path]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let api = dir.0.join("runtime/ratelimit/config/api.yaml");
    let api = api.display();
    // Merged, `logon` names a limit of the domain.
    assert_eq!(
        stdout,
        format!(
            "rls: 2 files, 1 domain, 7 descriptors, 6 limits, 0 unlimited, 1 in shadow mode\n\
             {api}:10: warning[rls-zero-limit]: descriptor api.client: the limit allows 0 requests per second (requests_per_unit is missing), so every request it matches is over the limit\n\
             {api}:12: warning[rls-shadow-no-limit]: descriptor api.tenant: shadow_mode is set but the descriptor has no rate_limit, so there is nothing to shadow\n\
             {api}:16: warning[rls-wildcard-shadowed]: descriptor api.ua_curl*: never reached: the wildcard value '*' on line 13 matches every value this descriptor would, and is tried first\n\
             {path}: 3 warnings\n"
        )
    );
    assert_eq!(out.status.code(), Some(1));

    // Without merging, the second file's domain is refused; with dot files
    // read, the swap file does not parse.
    let path = dir.write("nomerge.toml", "[rls]\nruntime_root = \"runtime\"\nruntime_subdirectory = \"ratelimit\"\nruntime_ignore_dotfiles = true\n");
    let out = edgefront(&["check", &path]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .ends_with("ratelimit/config/more.yaml: duplicate domain 'api' in config file\n")
    );
    let path = dir.write(
        "dots.toml",
        "[rls]\nruntime_root = \"runtime\"\nruntime_subdirectory = \"ratelimit\"\n",
    );
    let out = edgefront(&["check", &path]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("ratelimit/config/.api.yaml.swp: error loading config file:"),
    );

    // Beside listeners, both are checked; unknown settings and a missing
    // directory are errors.
    let both = format!(
        "[[listeners]]\nname = \"a\"\nlisten = \"0.0.0.0:80\"\nupstream = \"10.0.0.1:80\"\nmax_conns = 100\nper_ip_max_conns = 100\n\n{toml}"
    );
    let path = dir.write("both.toml", &both);
    let stdout = String::from_utf8(edgefront(&["check", &path]).stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines[0].starts_with("rls: 2 files"), "{stdout}");
    assert!(
        lines[1].contains(":6: warning[per-ip-cap-unreachable]"),
        "{stdout}"
    );
    assert!(
        lines[2].contains("api.yaml:10: warning[rls-zero-limit]"),
        "{stdout}"
    );
    assert_eq!(*lines.last().unwrap(), format!("{path}: 4 warnings"));
    let path = dir.write("typo.toml", "[rls]\nruntime_rot = \"/x\"\n");
    let out = edgefront(&["check", &path]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("unknown field `runtime_rot`")
    );
    let path = dir.write("missing.toml", "[rls]\nruntime_root = \"nowhere\"\n");
    let out = edgefront(&["check", &path]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.contains(": rls: cannot read ") && err.contains("nowhere/config"),
        "{err}"
    );
}
