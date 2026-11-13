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
        "/../../legacy/pacer/examples/pacerd.conf"
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
        "/../../legacy/pacer/examples/pacerd.conf"
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
