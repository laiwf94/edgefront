//! The tests of pacer's `tests/test_config.c`, plus the serde form and
//! round trips. Messages are checked whole where test_config.c checked a
//! part, so a change in wording shows up here before the differential test.

use super::*;
use proptest::prelude::*;

const SEC_D: Duration = Duration::from_secs(1);

fn parse(text: &str) -> Result<LimiterConfig, ConfigError> {
    parse_pacerd(text.as_bytes())
}

fn error(text: &[u8]) -> String {
    match parse_pacerd(text) {
        Ok(cfg) => panic!("{:?} parsed: {cfg:?}", String::from_utf8_lossy(text)),
        Err(e) => e.to_string(),
    }
}

/// A one-listener config with `extra` on line 4 of the block.
fn listener_error(extra: &str) -> String {
    error(
        format!("listener a {{\n listen 127.0.0.1:1\n upstream 127.0.0.1:2\n {extra}\n}}\n")
            .as_bytes(),
    )
}

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

#[test]
fn config_full_example() {
    let text = "# pacerd example\n\
                workers 4\n\
                admin_socket /run/pacerd.sock\n\
                drain_timeout 10s\n\
                log_level debug\n\
                \n\
                listener web {   # public side\n\
                \tlisten 0.0.0.0:8080\n    \
                upstream [::1]:9000\r\n    \
                max_conns 5000\n    \
                buffer 32k\n    \
                connect_timeout 250ms\n    \
                idle_timeout 2m\n    \
                min_rate 64 per 10s\n    \
                per_ip_max_conns 20\n    \
                per_ip_conn_rate 10/s burst 20\n    \
                per_ip_bandwidth 1m/s burst 256k\n    \
                bandwidth 600/m\n    \
                table_size 4096\n    \
                ipv6_prefix 56\n    \
                table_full evict\n\
                }\n\
                listener ssh {\n    \
                listen [::]:2222\n    \
                upstream 10.0.0.2:22\n\
                }";
    let cfg = parse(text).unwrap();
    assert_eq!(cfg.workers, 4);
    assert_eq!(cfg.admin_socket, Some(PathBuf::from("/run/pacerd.sock")));
    assert_eq!(cfg.drain_timeout, 10 * SEC_D);
    assert_eq!(cfg.log_level, LogLevel::Debug);
    assert_eq!(cfg.listeners.len(), 2);
    let l = &cfg.listeners[0];
    assert_eq!(l.name, "web");
    assert_eq!(l.listen, addr("0.0.0.0:8080"));
    assert_eq!(l.upstream, addr("[::1]:9000"));
    assert_eq!(l.max_conns, 5000);
    assert_eq!(l.buffer_size, 32 * 1024);
    assert_eq!(l.connect_timeout, Duration::from_millis(250));
    assert_eq!(l.idle_timeout, 120 * SEC_D);
    assert_eq!(
        l.min_rate,
        Some(MinRate {
            bytes: 64,
            window: 10 * SEC_D
        })
    );
    assert_eq!(l.per_ip_max_conns, 20);
    assert_eq!(l.per_ip_conn_rate, Some(Rate::new(10, SEC, 20)));
    assert_eq!(l.per_ip_bandwidth, Some(Rate::new(1 << 20, SEC, 256 << 10)));
    assert_eq!(l.bandwidth, Some(Rate::new(600, 60 * SEC, 600)));
    assert_eq!(l.table_size, 4096);
    assert_eq!(l.ipv6_prefix, 56);
    assert_eq!(l.table_full, FullPolicy::Evict);
    let l = &cfg.listeners[1];
    assert_eq!(
        *l,
        ListenerConfig::new("ssh", addr("[::]:2222"), addr("10.0.0.2:22"))
    );
    assert_eq!(l.max_conns, 1024);
    assert_eq!(l.buffer_size, 16 * 1024);
    assert_eq!(l.per_ip_conn_rate, None);
    assert_eq!(l.min_rate, None);
    assert_eq!(l.table_full, FullPolicy::Deny);
}

#[test]
fn config_defaults() {
    let cfg = parse("listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n}\n").unwrap();
    assert_eq!(cfg.workers, 1);
    assert_eq!(cfg.drain_timeout, 30 * SEC_D);
    assert_eq!(cfg.log_level, LogLevel::Info);
    assert_eq!(cfg.admin_socket, None);
    let l = &cfg.listeners[0];
    assert_eq!(l.connect_timeout, 5 * SEC_D);
    assert_eq!(l.idle_timeout, 300 * SEC_D);
    assert_eq!(l.ipv6_prefix, 64);
    assert_eq!(l.table_size, 16_384);
    assert_eq!(
        cfg,
        LimiterConfig {
            listeners: vec![ListenerConfig::new(
                "a",
                addr("127.0.0.1:1"),
                addr("127.0.0.1:2")
            )],
            ..LimiterConfig::default()
        }
    );
}

#[test]
fn config_rate_units() {
    let cfg = parse(
        "listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n\
         per_ip_conn_rate 100/h\nbandwidth 1g/s burst 1m\n}\n",
    )
    .unwrap();
    let l = &cfg.listeners[0];
    assert_eq!(l.per_ip_conn_rate, Some(Rate::new(100, 3600 * SEC, 100)));
    assert_eq!(l.bandwidth, Some(Rate::new(1 << 30, SEC, 1 << 20)));
}

#[test]
fn config_min_rate_off() {
    let cfg =
        parse("listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\nmin_rate off\n}\n").unwrap();
    assert_eq!(cfg.listeners[0].min_rate, None);
}

#[test]
fn config_value_errors() {
    let cases: &[(&str, &str)] = &[
        ("max_conns 0", "max_conns: must be between 1 and 1000000"),
        (
            "max_conns 1000001",
            "max_conns: must be between 1 and 1000000",
        ),
        (
            "max_conns -1",
            "max_conns: expected a whole number, got \"-1\"",
        ),
        (
            "max_conns 1.5",
            "max_conns: expected a whole number, got \"1.5\"",
        ),
        (
            "max_conns 0x10",
            "max_conns: expected a whole number, got \"0x10\"",
        ),
        (
            "max_conns 99999999999999999999999",
            "max_conns: expected a whole number, got \"99999999999999999999999\"",
        ),
        (
            "max_conns ten",
            "max_conns: expected a whole number, got \"ten\"",
        ),
        (
            "buffer 512",
            "buffer: must be between 1024 and 1048576 bytes",
        ),
        (
            "buffer 2m",
            "buffer: must be between 1024 and 1048576 bytes",
        ),
        (
            "buffer 16q",
            "buffer: expected a size like 16k, got \"16q\"",
        ),
        ("buffer k", "buffer: expected a size like 16k, got \"k\""),
        (
            "buffer 99999999999999999g",
            "buffer: expected a size like 16k, got \"99999999999999999g\"",
        ),
        (
            "connect_timeout 5",
            "connect_timeout: expected a duration like 5s or 250ms, got \"5\"",
        ),
        (
            "connect_timeout 5d",
            "connect_timeout: expected a duration like 5s or 250ms, got \"5d\"",
        ),
        (
            "connect_timeout 0ms",
            "connect_timeout: must be between 1ms and 600000ms",
        ),
        (
            "idle_timeout 99999999999h",
            "idle_timeout: expected a duration like 5s or 250ms, got \"99999999999h\"",
        ),
        (
            "idle_timeout 2d",
            "idle_timeout: expected a duration like 5s or 250ms, got \"2d\"",
        ),
        (
            "per_ip_conn_rate 10",
            "per_ip_conn_rate: expected a rate like 10/s, 600/m or 1m/s, got \"10\"",
        ),
        (
            "per_ip_conn_rate 0/s",
            "per_ip_conn_rate: expected a rate like 10/s, 600/m or 1m/s, got \"0/s\"",
        ),
        (
            "per_ip_conn_rate 10/d",
            "per_ip_conn_rate: expected a rate like 10/s, 600/m or 1m/s, got \"10/d\"",
        ),
        (
            "per_ip_conn_rate x/s",
            "per_ip_conn_rate: expected a rate like 10/s, 600/m or 1m/s, got \"x/s\"",
        ),
        (
            "per_ip_conn_rate 99999999999999999999999999999999/s",
            "per_ip_conn_rate: expected a rate like 10/s, 600/m or 1m/s, \
             got \"99999999999999999999999999999999/s\"",
        ),
        (
            "per_ip_conn_rate 10/s burst",
            "per_ip_conn_rate: expected RATE or RATE burst N",
        ),
        (
            "per_ip_conn_rate 10/s bust 5",
            "per_ip_conn_rate: expected \"burst\", got \"bust\"",
        ),
        (
            "per_ip_conn_rate 10/s burst 0",
            "per_ip_conn_rate: expected a burst like 20 or 64k, got \"0\"",
        ),
        (
            "per_ip_conn_rate 10/s burst many",
            "per_ip_conn_rate: expected a burst like 20 or 64k, got \"many\"",
        ),
        (
            "per_ip_conn_rate 1/h burst 100000",
            "per_ip_conn_rate: rate and burst are too large together",
        ),
        (
            "min_rate 64",
            "min_rate: expected BYTES per DURATION, or off",
        ),
        (
            "min_rate 64 over 10s",
            "min_rate: expected BYTES per DURATION, or off",
        ),
        (
            "min_rate 0 per 10s",
            "min_rate: expected a byte count of at least 1, got \"0\"",
        ),
        (
            "min_rate 64 per 100ms",
            "min_rate: expected a window between 1s and 10m, got \"100ms\"",
        ),
        (
            "min_rate 64 per 11m",
            "min_rate: expected a window between 1s and 10m, got \"11m\"",
        ),
        (
            "table_full maybe",
            "table_full: expected deny or evict, got \"maybe\"",
        ),
        (
            "table_size 8",
            "table_size: must be between 16 and 16777216",
        ),
        ("ipv6_prefix 16", "ipv6_prefix: must be between 32 and 128"),
        ("ipv6_prefix 129", "ipv6_prefix: must be between 32 and 128"),
    ];
    for (extra, want) in cases {
        assert_eq!(
            listener_error(extra),
            format!("line 4: {want}"),
            "for {extra:?}"
        );
    }
}

#[test]
fn config_address_errors() {
    let long6 = format!("[{}]:1", ["0000"; 13].join(":"));
    let long4 = format!("{}:1", "0".repeat(70));
    let cases = [
        "127.0.0.1",
        "127.0.0.1:0",
        "127.0.0.1:65536",
        "localhost:80",
        "::1:80",
        "[::1]80",
        "[::1:80",
        "[127.0.0.1]:80",
        "1.2.3.4:http",
        "01.2.3.4:80",
        "1.2.3:80",
        "[::1]:",
        ":80",
        "[]:80",
        "[::1%1]:80",
        "[12345::]:80",
        long6.as_str(),
        long4.as_str(),
    ];
    for a in cases {
        let got = error(format!("listener a {{\nlisten {a}\n}}\n").as_bytes());
        let want = format!(
            "line 2: listen: expected an address like 127.0.0.1:8080 or [::1]:8080, got \"{}\"",
            shown(a.as_bytes())
        );
        assert_eq!(got, want, "for {a:?}");
    }
}

#[test]
fn addresses_that_parse() {
    for (text, want) in [
        ("1.2.3.4:1", "1.2.3.4:1"),
        ("255.255.255.255:65535", "255.255.255.255:65535"),
        ("0.0.0.0:00080", "0.0.0.0:80"),
        ("[::]:1", "[::]:1"),
        ("[::ffff:1.2.3.4]:80", "[::ffff:1.2.3.4]:80"),
        ("[2001:DB8::1]:8443", "[2001:db8::1]:8443"),
        ("[1:2:3:4:5:6:7::]:9", "[1:2:3:4:5:6:7:0]:9"),
    ] {
        assert_eq!(parse_addr(text.as_bytes()), Some(addr(want)), "{text}");
    }
    assert_eq!(parse_addr(&[b'1', 0xff, b':', b'1']), None);
}

#[test]
fn config_structure_errors() {
    let cases: &[(&str, &str)] = &[
        ("", "line 0: no listener defined"),
        ("# only a comment\n", "line 1: no listener defined"),
        (
            "workers 2\nworkers 3\nlistener a {\n}\n",
            "line 2: workers set twice",
        ),
        ("workers 0\n", "line 1: workers: must be between 1 and 64"),
        ("workers 1 2\n", "line 1: workers: wrong number of values"),
        ("workers\n", "line 1: workers: wrong number of values"),
        ("frobnicate 1\n", "line 1: unknown directive \"frobnicate\""),
        (
            "listen 127.0.0.1:1\n",
            "line 1: listen belongs inside a listener block",
        ),
        (
            "listener a {\nworkers 2\n}\n",
            "line 2: workers belongs outside listener blocks",
        ),
        (
            "listener a {\nlistener b {\n",
            "line 2: listener blocks cannot be nested",
        ),
        ("listener a\n", "line 1: expected: listener NAME {"),
        ("listener a b {\n", "line 1: expected: listener NAME {"),
        ("listener a {{\n", "line 1: expected: listener NAME {"),
        (
            "listener bad.name {\n",
            "line 1: listener name \"bad.name\" must be 1-31 letters, digits, _ or -",
        ),
        (
            "listener aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa {\n",
            "line 1: listener name \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\" must be 1-31 letters, \
             digits, _ or -",
        ),
        ("}\n", "line 1: unexpected }"),
        (
            "listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n} x\n",
            "line 4: unexpected }",
        ),
        (
            "listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n",
            "line 3: listener a: missing }",
        ),
        (
            "listener a {\nupstream 127.0.0.1:2\n}\n",
            "line 3: listener a: missing listen",
        ),
        (
            "listener a {\nlisten 127.0.0.1:2\n}\n",
            "line 3: listener a: missing upstream",
        ),
        (
            "listener a {\nlisten 127.0.0.1:1\nlisten 127.0.0.1:3\n}\n",
            "line 3: listen set twice",
        ),
        (
            "listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n}\nlistener a {\n",
            "line 5: listener a defined twice",
        ),
        (
            "listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n}\n\
             listener b {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n}\n",
            "line 8: listener b: listen address already used by a",
        ),
        ("a b c d e f g h i\n", "line 1: too many words on one line"),
        (
            "log_level loud\n",
            "line 1: log_level: expected debug, info, warn or error, got \"loud\"",
        ),
        (
            "admin_socket relative.sock\n",
            "line 1: admin_socket: expected an absolute path shorter than 108 bytes, \
             got \"relative.sock\"",
        ),
        (
            "drain_timeout 2h\n",
            "line 1: drain_timeout: must be between 0ms and 3600000ms",
        ),
    ];
    for (text, want) in cases {
        assert_eq!(error(text.as_bytes()), *want, "for {text:?}");
    }
}

#[test]
fn words_and_comments() {
    // Eight words is the limit, and a comment after them is fine.
    assert_eq!(
        error(b"a b c d e f g h # nine\n"),
        "line 1: unknown directive \"a\""
    );
    // A comment may start mid-word, and ends the word.
    let cfg = parse(
        "workers 3#four\nlistener a{ #\nlistener a {\nlisten 127.0.0.1:1#x\nupstream 127.0.0.1:2\n}#\n",
    );
    assert_eq!(
        cfg.unwrap_err().to_string(),
        "line 2: expected: listener NAME {"
    );
    let cfg =
        parse("workers 3#four\nlistener a {#\nlisten 127.0.0.1:1#x\nupstream\t127.0.0.1:2\r\n}#\n")
            .unwrap();
    assert_eq!(cfg.workers, 3);
    assert_eq!(cfg.listeners[0].listen, addr("127.0.0.1:1"));
    // Vertical tabs and form feeds are not spaces to pacerd.
    assert_eq!(
        error(b"workers\x0b2\n"),
        "line 1: unknown directive \"workers?2\""
    );
    assert_eq!(tokenize(b"  \t\r "), Some(vec![]));
    assert_eq!(tokenize(b"a b c d e f g h i"), None);
    assert_eq!(tokenize(b"a#b"), Some(vec![&b"a"[..]]));
}

#[test]
fn config_top_level_duplicates_span_blocks() {
    assert_eq!(
        error(b"workers 2\nlistener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n}\nworkers 3\n"),
        "line 6: workers set twice"
    );
    // Directives inside each block are tracked separately.
    let cfg = parse(
        "listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n}\n\
         listener b {\nlisten 127.0.0.1:3\nupstream 127.0.0.1:2\n}\n",
    )
    .unwrap();
    assert_eq!(cfg.listeners.len(), 2);
}

#[test]
fn config_hostile_input() {
    assert_eq!(
        error(b"workers 1\nwork\0ers 2\n"),
        "line 2: unexpected NUL byte"
    );
    let long = vec![b'a'; LINE_MAX + 9];
    assert_eq!(error(&long), "line 1: line longer than 1024 bytes");
    // A line of exactly the limit is read.
    let mut exact = vec![b'#'; LINE_MAX];
    exact.extend_from_slice(b"\nworkers 0\n");
    assert_eq!(error(&exact), "line 2: workers: must be between 1 and 64");

    // Control bytes from the file never reach the message.
    assert_eq!(
        error(b"\x1b[31mevil 1\n"),
        "line 1: unknown directive \"?[31mevil\""
    );
    assert_eq!(
        error(b"listener a {\nmax_conns \"7\n"),
        "line 2: max_conns: expected a whole number, got \"?7\""
    );
    // Bytes above 0x7f are replaced too.
    assert_eq!(
        error(b"\xc3\xa9vil\xff 1\n"),
        "line 1: unknown directive \"??vil?\""
    );
    // Long tokens are cut short in messages, at 44 bytes.
    let a66 = "a".repeat(66);
    assert_eq!(
        error(format!("{a66} 1\n").as_bytes()),
        format!("line 1: unknown directive \"{}...\"", "a".repeat(44))
    );
    let a44 = "a".repeat(44);
    assert_eq!(
        error(format!("{a44} 1\n").as_bytes()),
        format!("line 1: unknown directive \"{a44}\"")
    );

    let huge = vec![0u8; MAX_FILE_BYTES + 1];
    assert_eq!(error(&huge), "line 0: config larger than 1048576 bytes");
}

#[test]
fn line_numbers_at_end_of_file() {
    // The last line counts whether or not it ends in a newline, and a
    // final newline does not start another line.
    assert_eq!(error(b"\n\n"), "line 2: no listener defined");
    assert_eq!(error(b"\n\nworkers 2"), "line 3: no listener defined");
    assert_eq!(error(b"listener a {\n\n"), "line 2: listener a: missing }");
}

#[test]
fn sixteen_listeners_at_most() {
    let mut text = String::new();
    for i in 0..MAX_LISTENERS {
        text.push_str(&format!(
            "listener l{i} {{\nlisten 127.0.0.1:{}\nupstream 127.0.0.1:1\n}}\n",
            i + 1
        ));
    }
    assert_eq!(parse(&text).unwrap().listeners.len(), MAX_LISTENERS);
    text.push_str("listener extra {\n");
    assert_eq!(error(text.as_bytes()), "line 65: at most 16 listeners");
}

#[test]
fn value_parsers_match_config_c() {
    assert_eq!(parse_u64(b""), None);
    assert_eq!(parse_u64(b"18446744073709551615"), Some(u64::MAX));
    assert_eq!(parse_u64(b"18446744073709551616"), None);
    assert_eq!(parse_u64(b"007"), Some(7));
    // At most 23 digits before a unit, even zeros.
    assert_eq!(parse_size(&[b'0'; 23]), Some(0));
    assert_eq!(parse_size(&[b'0'; 24]), None);
    assert_eq!(parse_size(b"1g"), Some(1 << 30));
    assert_eq!(parse_size(b"1048576g"), Some(1 << 50));
    assert_eq!(parse_size(b"1048577g"), None);
    assert_eq!(parse_size(b"1125899906842624"), Some(1 << 50));
    assert_eq!(parse_size(b"1125899906842625"), None);
    assert_eq!(parse_size(b"1K"), None);
    assert_eq!(parse_size(b"1kb"), None);
    assert_eq!(parse_duration(b"24h"), Some(MAX_DUR));
    assert_eq!(parse_duration(b"1441m"), None);
    assert_eq!(parse_duration(b"0ms"), Some(0));
    assert_eq!(parse_duration(b"1"), None);
    assert_eq!(parse_rate(b"1m/s"), Some(Rate::new(1 << 20, SEC, 1 << 20)));
    assert_eq!(parse_rate(b"10/s/s"), None);
    assert_eq!(parse_rate(b"/s"), None);
    assert_eq!(
        parse_rate(&[b"0000000000000000000000000000001".as_slice(), b"/s"].concat()),
        None
    );
    assert_eq!(fmt_size(0), "0");
    assert_eq!(fmt_size(1536), "1536");
    assert_eq!(fmt_size(3 << 20), "3m");
    assert_eq!(fmt_size(1 << 50), "1048576g");
}

#[test]
fn reads_files() {
    let dir = std::env::temp_dir().join(format!("edgefront-limiter-config-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("pacerd.conf");
    std::fs::write(
        &path,
        "listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n}\n",
    )
    .unwrap();
    assert_eq!(read_pacerd(&path).unwrap().listeners.len(), 1);

    // Oversized files are refused after reading one byte past the limit.
    std::fs::write(&path, "#########\n".repeat(MAX_FILE_BYTES / 10 + 2)).unwrap();
    assert_eq!(
        read_pacerd(&path).unwrap_err(),
        "line 0: config larger than 1048576 bytes"
    );
    std::fs::write(&path, "bogus\n").unwrap();
    assert_eq!(
        read_pacerd(&path).unwrap_err(),
        "line 1: unknown directive \"bogus\""
    );
    let missing = dir.join("missing.conf");
    let e = read_pacerd(&missing).unwrap_err();
    assert!(e.starts_with(&format!("{}: ", missing.display())), "{e}");
    assert!(e.contains("No such file"), "{e}");
    assert!(read_pacerd(&dir).unwrap_err().contains("Is a directory"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn reads_pacers_own_files() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../legacy/pacer");
    let example = read_pacerd(Path::new(&format!("{root}/examples/pacerd.conf"))).unwrap();
    assert_eq!(example.workers, 2);
    assert_eq!(
        example.listeners[0].bandwidth,
        Some(Rate::new(100 << 20, SEC, 1 << 20))
    );
    for name in ["full.conf", "minimal.conf", "two.conf"] {
        let path = format!("{root}/fuzz/corpus/config/{name}");
        let cfg = read_pacerd(Path::new(&path)).unwrap();
        let back = parse_pacerd(&cfg.to_pacerd().unwrap()).unwrap();
        assert_eq!(back, cfg, "{name}");
    }
}

// ---------------------------------------------------------------------------
// The serde form.

fn from_toml(text: &str) -> Result<LimiterConfig, String> {
    toml::from_str(text).map_err(|e| e.message().to_owned())
}

const EXAMPLE_TOML: &str = r#"
workers = 2
admin_socket = "/run/pacerd/admin.sock"
drain_timeout = "30s"
log_level = "info"

[[listeners]]
name = "web"
listen = "0.0.0.0:8080"
upstream = "127.0.0.1:9000"
max_conns = 10000
bandwidth = "100m/s burst 1m"
per_ip_max_conns = 20
per_ip_conn_rate = "10/s burst 20"
per_ip_bandwidth = "1m/s burst 256k"
min_rate = "512 per 10s"
idle_timeout = "5m"
connect_timeout = "5s"
buffer = "16k"
table_size = 65536
table_full = "deny"
"#;

#[test]
fn toml_matches_pacerd_conf() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../legacy/pacer");
    let example = read_pacerd(Path::new(&format!("{root}/examples/pacerd.conf"))).unwrap();
    assert_eq!(from_toml(EXAMPLE_TOML).unwrap(), example);
    // Sizes and counts may be integers or strings.
    let cfg = from_toml(
        "[[listeners]]\nname = \"a\"\nlisten = \"127.0.0.1:1\"\nupstream = \"[::1]:2\"\n\
         buffer = 2048\nmax_conns = \"7\"\n",
    )
    .unwrap();
    assert_eq!(cfg.listeners[0].buffer_size, 2048);
    assert_eq!(cfg.listeners[0].max_conns, 7);
    assert_eq!(cfg.workers, 1);
}

#[test]
fn toml_errors_name_the_field() {
    let base =
        "[[listeners]]\nname = \"a\"\nlisten = \"127.0.0.1:1\"\nupstream = \"127.0.0.1:2\"\n";
    let with = |extra: &str| from_toml(&format!("{base}{extra}\n")).unwrap_err();
    let cases: &[(&str, &str)] = &[
        (
            "max_conns = 0",
            "listeners[0].max_conns: must be between 1 and 1000000",
        ),
        (
            "max_conns = -1",
            "listeners[0].max_conns: expected a whole number, got \"-1\"",
        ),
        (
            "buffer = \"16q\"",
            "listeners[0].buffer: expected a size like 16k, got \"16q\"",
        ),
        (
            "connect_timeout = 5",
            "listeners[0].connect_timeout: expected a duration like 5s or 250ms, got \"5\"",
        ),
        (
            "per_ip_conn_rate = \"10/s burst\"",
            "listeners[0].per_ip_conn_rate: expected RATE or RATE burst N, like 10/s burst 20, got \"10/s burst\"",
        ),
        (
            "min_rate = \"\"",
            "listeners[0].min_rate: wrong number of values",
        ),
        (
            "min_rate = \"1 per 1s x y\"",
            "listeners[0].min_rate: wrong number of values",
        ),
        (
            "table_full = \"maybe\"",
            "listeners[0].table_full: expected deny or evict, got \"maybe\"",
        ),
    ];
    for (extra, want) in cases {
        assert_eq!(with(extra), *want, "for {extra:?}");
    }
    let top: &[(&str, &str)] = &[
        ("workers = 65", "workers: must be between 1 and 64"),
        (
            "admin_socket = \"x.sock\"",
            "admin_socket: expected an absolute path shorter than 108 bytes, got \"x.sock\"",
        ),
        (
            "log_level = \"loud\"",
            "log_level: expected debug, info, warn or error, got \"loud\"",
        ),
        (
            "drain_timeout = \"2h\"",
            "drain_timeout: must be between 0ms and 3600000ms",
        ),
    ];
    for (line, want) in top {
        assert_eq!(
            from_toml(&format!("{line}\n{base}")).unwrap_err(),
            *want,
            "for {line:?}"
        );
    }
    let structure: &[(&str, &str)] = &[
        ("workers = 2\n", "listeners: no listener defined"),
        (
            "[[listeners]]\nlisten = \"127.0.0.1:1\"\n",
            "listeners[0].name: missing",
        ),
        (
            "[[listeners]]\nname = \"a.b\"\n",
            "listeners[0]: listener name \"a.b\" must be 1-31 letters, digits, _ or -",
        ),
        (
            "[[listeners]]\nname = \"a\"\nupstream = \"127.0.0.1:2\"\n",
            "listeners[0]: listener a: missing listen",
        ),
        (
            "[[listeners]]\nname = \"a\"\nlisten = \"127.0.0.1:2\"\n",
            "listeners[0]: listener a: missing upstream",
        ),
        (
            &format!("{base}{base}"),
            "listeners[1]: listener a defined twice",
        ),
        (
            &format!(
                "{base}[[listeners]]\nname = \"b\"\nlisten = \"127.0.0.1:1\"\nupstream = \"127.0.0.1:2\"\n"
            ),
            "listeners[1]: listener b: listen address already used by a",
        ),
    ];
    for (text, want) in structure {
        assert_eq!(from_toml(text).unwrap_err(), *want, "for {text:?}");
    }
    // Unknown fields and wrong types are serde's to report.
    assert!(
        from_toml(&format!("{base}bogus = 1\n"))
            .unwrap_err()
            .contains("unknown field `bogus`")
    );
    assert!(
        from_toml("frobnicate = 1\n")
            .unwrap_err()
            .contains("unknown field `frobnicate`")
    );
    assert!(
        from_toml(&format!("{base}max_conns = true\n"))
            .unwrap_err()
            .contains("a string or an integer")
    );
    let many: String = (0..17)
        .map(|i| format!("[[listeners]]\nname = \"l{i}\"\nlisten = \"127.0.0.1:{}\"\nupstream = \"127.0.0.1:1\"\n", i + 1))
        .collect();
    assert_eq!(
        from_toml(&many).unwrap_err(),
        "listeners[16]: at most 16 listeners"
    );
}

#[test]
fn toml_admin_socket_is_one_word() {
    let cfg = from_toml(
        "admin_socket = \"/run/my dir/admin.sock\"\n[[listeners]]\nname = \"a\"\n\
         listen = \"127.0.0.1:1\"\nupstream = \"127.0.0.1:2\"\n",
    )
    .unwrap();
    assert_eq!(
        cfg.admin_socket,
        Some(PathBuf::from("/run/my dir/admin.sock"))
    );
    // It serializes, but pacerd's syntax cannot hold it.
    assert!(
        toml::to_string(&cfg)
            .unwrap()
            .contains("/run/my dir/admin.sock")
    );
    assert!(
        cfg.to_pacerd()
            .unwrap_err()
            .contains("cannot be written in pacerd syntax")
    );
    let nul = LimiterConfig {
        admin_socket: Some(PathBuf::from("/a\0b")),
        ..cfg
    };
    assert!(
        nul.validate()
            .unwrap_err()
            .starts_with("admin_socket: expected an absolute path")
    );
}

#[test]
fn configs_built_in_code_are_checked() {
    let l = ListenerConfig::new("a", addr("127.0.0.1:1"), addr("127.0.0.1:2"));
    let ok = LimiterConfig {
        listeners: vec![l.clone()],
        ..LimiterConfig::default()
    };
    assert_eq!(ok.validate(), Ok(()));
    let with = |f: &dyn Fn(&mut ListenerConfig)| {
        let mut l = l.clone();
        f(&mut l);
        let cfg = LimiterConfig {
            listeners: vec![l],
            ..LimiterConfig::default()
        };
        let v = cfg.validate().unwrap_err();
        assert_eq!(cfg.to_pacerd().unwrap_err(), v);
        assert!(
            toml::to_string(&cfg).unwrap_err().to_string().contains(&v),
            "{v}"
        );
        v
    };
    assert_eq!(
        with(&|l| l.connect_timeout = Duration::from_micros(1500)),
        "listeners[0].connect_timeout: 1.5ms is not a whole number of milliseconds up to 24h"
    );
    assert_eq!(
        with(&|l| l.idle_timeout = Duration::from_secs(86_401)),
        "listeners[0].idle_timeout: 86401s is not a whole number of milliseconds up to 24h"
    );
    assert_eq!(
        with(&|l| l.idle_timeout = Duration::ZERO),
        "listeners[0].idle_timeout: must be between 1000ms and 86400000ms"
    );
    assert_eq!(
        with(&|l| l.bandwidth = Some(Rate::new(1, 1500 * MS, 1))),
        "listeners[0].bandwidth: a period of 1500000000ns is not a whole number of seconds"
    );
    assert_eq!(
        with(&|l| l.bandwidth = Some(Rate::new(1, 0, 1))),
        "listeners[0].bandwidth: a period of 0ns is not a whole number of seconds"
    );
    assert_eq!(
        with(&|l| l.bandwidth = Some(Rate::new(1 << 51, 2 * SEC, 1))),
        "listeners[0].bandwidth: \"2251799813685248/2s\": the count is above 2^50"
    );
    for (limit, window) in [
        (5, Duration::from_secs(1800)),
        (0, Duration::from_secs(3600)),
        ((1 << 50) + 1, Duration::from_secs(3600)),
        (5, Duration::from_millis(3_600_500)),
        (5, Duration::from_secs(367 * 86_400)),
    ] {
        let v = with(&|l| l.per_ip_conn_quota = Some(ConnQuota { limit, window }));
        assert_eq!(
            v,
            format!(
                "listeners[0].per_ip_conn_quota: {limit} in {window:?} is not a quota: the limit must be 1 to 2^50 and the window a whole number of seconds from 1h to 366 days"
            )
        );
    }
    assert_eq!(
        with(&|l| l.per_ip_bandwidth = Some(Rate::new(1, 3600 * SEC, 1 << 50))),
        "listeners[0].per_ip_bandwidth: \"1/h\": rate and burst are too large together"
    );
    assert_eq!(
        with(&|l| {
            l.min_rate = Some(MinRate {
                bytes: 1,
                window: Duration::from_millis(999),
            })
        }),
        "listeners[0].min_rate: expected a window between 1s and 10m, got \"999ms\""
    );
    assert_eq!(
        with(&|l| l.name = String::new()),
        "listeners[0]: listener name \"\" must be 1-31 letters, digits, _ or -"
    );
    assert_eq!(
        with(&|l| l.upstream = "[fe80::1%2]:80".parse().unwrap()),
        "listeners[0].upstream: [fe80::1%2]:80 has a flow label or scope id, which cannot be configured"
    );
    assert_eq!(
        with(&|l| l.buffer_size = 0),
        "listeners[0].buffer: must be between 1024 and 1048576 bytes"
    );
    let none = LimiterConfig::default();
    assert_eq!(
        none.validate().unwrap_err(),
        "listeners: no listener defined"
    );
    let bad_drain = LimiterConfig {
        drain_timeout: Duration::from_nanos(1),
        ..ok
    };
    assert!(
        bad_drain
            .validate()
            .unwrap_err()
            .starts_with("drain_timeout: 1ns")
    );
}

fn one_listener(extra: &str) -> Result<LimiterConfig, String> {
    from_toml(&format!(
        "[[listeners]]\nname = \"a\"\nlisten = \"127.0.0.1:1\"\nupstream = \"127.0.0.1:2\"\n{extra}\n"
    ))
}

#[test]
fn toml_rates_take_fractions_and_units_up_to_days() {
    let rate = |field: &str, text: &str| {
        let cfg = one_listener(&format!("{field} = \"{text}\"")).unwrap();
        let l = &cfg.listeners[0];
        match field {
            "per_ip_conn_rate" => l.per_ip_conn_rate,
            "per_ip_bandwidth" => l.per_ip_bandwidth,
            _ => l.bandwidth,
        }
        .unwrap()
    };
    let cases: &[(&str, Rate)] = &[
        // pacerd's text is read as pacerd reads it.
        ("10/s burst 20", Rate::new(10, SEC, 20)),
        ("600/m", Rate::new(600, 60 * SEC, 600)),
        ("1m/s burst 256k", Rate::new(1 << 20, SEC, 256 << 10)),
        // RateSpec text, with the count over the period in lowest terms.
        ("0.5/s", Rate::new(1, 2 * SEC, 1)),
        ("90/min", Rate::new(90, 60 * SEC, 90)),
        ("2.5/10s burst 5", Rate::new(5, 20 * SEC, 5)),
        ("1.5m/s", Rate::new(3 << 19, SEC, 3 << 19)),
        ("3/2h burst 3", Rate::new(3, 7200 * SEC, 3)),
        ("10/day burst 10", Rate::new(10, 86_400 * SEC, 10)),
        ("0.25/day burst 1", Rate::new(1, 4 * 86_400 * SEC, 1)),
        ("1.000000001/19s burst 2", Rate::new(0, 0, 0)),
    ];
    for &(text, want) in &cases[..cases.len() - 1] {
        for field in ["per_ip_conn_rate", "per_ip_bandwidth", "bandwidth"] {
            assert_eq!(rate(field, text), want, "{field} = {text:?}");
        }
    }
    // A rate whose period would need more than 64 bits of nanoseconds to
    // hold its count exactly. RateSpec itself takes it, in lower terms
    // than whole seconds can write.
    assert!(RateSpec::parse(cases[cases.len() - 1].0).is_ok());
    let refused: &[(&str, &str)] = &[
        (
            "per_ip_conn_rate = \"1.000000001/19s burst 2\"",
            "listeners[0].per_ip_conn_rate: \"1.000000001/19s burst 2\" is too precise to hold exactly; use fewer decimal places",
        ),
        (
            "per_ip_conn_rate = \"10000/day\"",
            "listeners[0].per_ip_conn_rate: \"10000/day\" is a quota (a period of an hour or more with no burst), not a rate; set it as per_ip_conn_quota, or give it a burst to make it a token bucket",
        ),
        (
            "bandwidth = \"1g/day\"",
            "listeners[0].bandwidth: \"1073741824/day\" is a quota (a period of an hour or more with no burst), not a rate; give it a burst to make it a token bucket",
        ),
        (
            "bandwidth = \"0.0000000001/s\"",
            "listeners[0].bandwidth: \"0.0000000001/s\": a count may have at most 9 decimal places",
        ),
        (
            "per_ip_bandwidth = \"0.5/s burst 1048576g\"",
            "listeners[0].per_ip_bandwidth: \"0.5/s\": rate and burst are too large together",
        ),
        (
            "per_ip_bandwidth = \"1.5g/s\"",
            "listeners[0].per_ip_bandwidth: \"1.5g/s\": too fast to enforce within 0.1% (it would run 0.66% slow)",
        ),
        (
            "bandwidth = \"10/fortnight\"",
            "listeners[0].bandwidth: expected a rate like 0.5/s, 90/min or 10000/day, got \"10/fortnight\"",
        ),
        // A value that is no rate keeps pacerd's message.
        (
            "bandwidth = 10",
            "listeners[0].bandwidth: expected a rate like 10/s, 600/m or 1m/s, got \"10\"",
        ),
    ];
    for (extra, want) in refused {
        assert_eq!(one_listener(extra).unwrap_err(), *want, "for {extra:?}");
    }
}

#[test]
fn toml_takes_a_per_client_connection_quota() {
    let quota = |text: &str| {
        one_listener(&format!("per_ip_conn_quota = \"{text}\""))
            .map(|c| c.listeners[0].per_ip_conn_quota.unwrap())
    };
    let day = Duration::from_secs(86_400);
    let hour = Duration::from_secs(3_600);
    assert_eq!(
        quota("10000/day"),
        Ok(ConnQuota {
            limit: 10_000,
            window: day
        })
    );
    assert_eq!(
        quota("500/6h"),
        Ok(ConnQuota {
            limit: 500,
            window: 6 * hour
        })
    );
    assert_eq!(
        quota("1.5/h"),
        Ok(ConnQuota {
            limit: 3,
            window: 2 * hour
        })
    );
    assert_eq!(
        quota("90/90min"),
        Ok(ConnQuota {
            limit: 90,
            window: Duration::from_secs(5_400)
        })
    );
    assert_eq!(
        quota("100/min").unwrap_err(),
        "listeners[0].per_ip_conn_quota: \"100/min\" is a rate, not a quota: a quota has a period of an hour or more and no burst, like 10000/day; set a rate as per_ip_conn_rate"
    );
    assert_eq!(
        quota("100/day burst 5").unwrap_err(),
        "listeners[0].per_ip_conn_quota: \"100/day burst 5\" is a rate, not a quota: a quota has a period of an hour or more and no burst, like 10000/day; set a rate as per_ip_conn_rate"
    );
    assert_eq!(
        quota("10000/year").unwrap_err(),
        "listeners[0].per_ip_conn_quota: expected a rate like 0.5/s, 90/min or 10000/day, got \"10000/year\""
    );
    assert!(
        one_listener("per_ip_conn_quota = 5")
            .unwrap_err()
            .contains("expected a string")
    );
    // Written back in the largest unit that divides the window.
    for (limit, secs, text) in [
        (1, 86_400, "1/day"),
        (7, 2 * 86_400, "7/2day"),
        (3, 7_200, "3/2h"),
        (90, 5_400, "90/90min"),
        (4, 3_601, "4/3601s"),
    ] {
        let q = ConnQuota {
            limit,
            window: Duration::from_secs(secs),
        };
        assert_eq!(q.to_text().unwrap(), text);
        assert_eq!(ConnQuota::parse(text), Ok(q));
    }
}

#[test]
fn serde_only_settings_round_trip_but_are_not_pacerd() {
    let cfg = one_listener(
        "per_ip_conn_rate = \"0.5/s burst 3\"\nper_ip_conn_quota = \"10000/day\"\n\
         bandwidth = \"2.5/10s burst 5\"\nper_ip_bandwidth = \"10/day burst 10\"",
    )
    .unwrap();
    assert_eq!(cfg.validate(), Ok(()));
    let text = toml::to_string(&cfg).unwrap();
    assert!(
        text.contains("per_ip_conn_rate = \"1/2s burst 3\""),
        "{text}"
    );
    assert!(text.contains("per_ip_conn_quota = \"10000/day\""), "{text}");
    assert!(text.contains("bandwidth = \"5/20s burst 5\""), "{text}");
    assert!(
        text.contains("per_ip_bandwidth = \"10/day burst 10\""),
        "{text}"
    );
    assert_eq!(from_toml(&text).unwrap(), cfg);
    assert_eq!(
        cfg.to_pacerd().unwrap_err(),
        "listeners[0].per_ip_conn_rate: 1/2s burst 3 cannot be written in pacerd syntax, whose rates are per second, minute or hour"
    );
    let mut quota_only = cfg.clone();
    let l = &mut quota_only.listeners[0];
    (l.per_ip_conn_rate, l.per_ip_bandwidth, l.bandwidth) = (None, None, None);
    assert_eq!(
        quota_only.to_pacerd().unwrap_err(),
        "listeners[0].per_ip_conn_quota: pacerd has no quotas, so it cannot be written in pacerd syntax"
    );
    quota_only.listeners[0].per_ip_conn_quota = None;
    assert_eq!(
        parse_pacerd(&quota_only.to_pacerd().unwrap()).unwrap(),
        quota_only
    );
    // The line of a serde-only field is recorded like a directive's.
    let mut lines = ListenerLines::new(3);
    lines.set("per_ip_conn_quota", 9);
    lines.set("no_such_field", 10);
    assert_eq!(lines.line_of("per_ip_conn_quota"), 9);
    assert_eq!(lines.line_of("no_such_field"), 3);
    assert_eq!(
        rate_text(&Rate::new(3, 3_600 * SEC, 9)).unwrap(),
        "3/h burst 9"
    );
    assert_eq!(
        rate_text(&Rate::new(3, 7 * 86_400 * SEC, 9)).unwrap(),
        "3/7day burst 9"
    );
}

#[test]
fn display_and_error_trait() {
    let e = ConfigError {
        line: 3,
        message: "x".into(),
    };
    assert_eq!(e.to_string(), "line 3: x");
    let _: &dyn std::error::Error = &e;
    assert_eq!(LogLevel::default(), LogLevel::Info);
    for (level, name) in LogLevel::ALL {
        assert_eq!(level.as_str(), name);
    }
}

// ---------------------------------------------------------------------------
// Round trips over generated configs.

fn duration_ms(lo_ms: u64, hi_ms: u64) -> impl Strategy<Value = Duration> {
    prop_oneof![
        (lo_ms..=hi_ms).prop_map(Duration::from_millis),
        (lo_ms.div_ceil(1000)..=hi_ms / 1000).prop_map(Duration::from_secs),
        (lo_ms.div_ceil(60_000)..=hi_ms / 60_000).prop_map(|m| Duration::from_secs(m * 60)),
    ]
}

fn size(lo: u64, hi: u64) -> impl Strategy<Value = u64> {
    prop_oneof![
        lo..=hi,
        (0u32..=30).prop_flat_map(move |shift| {
            ((lo >> shift).max(1)..=(hi >> shift).max(1)).prop_map(move |v| v << shift)
        }),
    ]
    .prop_filter("in range", move |v| (lo..=hi).contains(v))
}

fn rate() -> impl Strategy<Value = Option<Rate>> {
    let period = prop_oneof![Just(SEC), Just(60 * SEC), Just(3600 * SEC)];
    proptest::option::of(
        (
            size(1, 1 << 40),
            period,
            proptest::option::of(size(1, 1 << 40)),
        )
            .prop_map(|(tokens, period, burst)| Rate::new(tokens, period, burst.unwrap_or(tokens)))
            .prop_filter("fits", |r| r.ticks().is_ok()),
    )
}

fn socket_addr() -> impl Strategy<Value = SocketAddr> {
    prop_oneof![
        (any::<[u8; 4]>(), 1u16..).prop_map(|(ip, port)| SocketAddr::from((ip, port))),
        (any::<[u16; 8]>(), 1u16..).prop_map(|(ip, port)| SocketAddr::from((ip, port))),
    ]
}

fn listener(i: usize) -> impl Strategy<Value = ListenerConfig> {
    let limits = (
        1u32..=1_000_000,
        size(1024, 1 << 20),
        duration_ms(1, 600_000),
        duration_ms(1000, 86_400_000),
        proptest::option::of((size(1, 1 << 50), duration_ms(1000, 600_000))),
        0u32..=1_000_000,
        32u32..=128,
    );
    let rest = (rate(), rate(), rate(), 16u32..=1 << 24, any::<bool>());
    (socket_addr(), socket_addr(), limits, rest).prop_map(
        move |(listen, upstream, limits, rest)| {
            let (max_conns, buffer, connect, idle, min_rate, per_ip, prefix) = limits;
            let (conn_rate, ip_bw, bw, table_size, evict) = rest;
            ListenerConfig {
                name: format!("l-{i}_x"),
                listen,
                upstream,
                max_conns,
                buffer_size: u32::try_from(buffer).unwrap(),
                connect_timeout: connect,
                idle_timeout: idle,
                min_rate: min_rate.map(|(bytes, window)| MinRate { bytes, window }),
                per_ip_max_conns: per_ip,
                ipv6_prefix: prefix,
                per_ip_conn_rate: conn_rate,
                per_ip_bandwidth: ip_bw,
                bandwidth: bw,
                per_ip_conn_quota: None,
                table_size,
                table_full: if evict {
                    FullPolicy::Evict
                } else {
                    FullPolicy::Deny
                },
            }
        },
    )
}

fn config() -> impl Strategy<Value = LimiterConfig> {
    let listeners = (1usize..=4).prop_flat_map(|n| (0..n).map(listener).collect::<Vec<_>>());
    (
        1u32..=64,
        proptest::option::of("/[a-z/._-]{0,100}"),
        duration_ms(0, 3_600_000),
        0usize..4,
        listeners,
    )
        .prop_map(|(workers, admin, drain, level, listeners)| LimiterConfig {
            workers,
            admin_socket: admin.map(PathBuf::from),
            drain_timeout: drain,
            log_level: LogLevel::ALL[level].0,
            listeners,
        })
        .prop_filter("listen addresses differ", |c| {
            c.listeners
                .iter()
                .enumerate()
                .all(|(i, l)| c.listeners[..i].iter().all(|o| o.listen != l.listen))
        })
}

/// A rate only the serde form can hold: a whole number of seconds that
/// is not one second, minute or hour, with tokens and period in lowest
/// terms, as the serde reader keeps them.
fn serde_rate() -> impl Strategy<Value = Rate> {
    (1u64..=1 << 30, 1u64..=366 * 86_400, 1u64..=1 << 30)
        .prop_map(|(tokens, secs, burst)| {
            let (mut a, mut b) = (tokens, secs * SEC);
            while b != 0 {
                (a, b) = (b, a % b);
            }
            Rate::new(tokens / a, secs * SEC / a, burst)
        })
        .prop_filter("whole seconds, not s, m or h, enforced within 0.1%", |r| {
            r.period_ns.is_multiple_of(SEC)
                && ![SEC, 60 * SEC, 3_600 * SEC].contains(&r.period_ns)
                && RateSpec::parse(&rate_text(r).unwrap()).is_ok()
        })
}

fn quota() -> impl Strategy<Value = ConnQuota> {
    (1u64..=1 << 50, 3_600u64..=366 * 86_400).prop_map(|(limit, secs)| ConnQuota {
        limit,
        window: Duration::from_secs(secs),
    })
}

/// [`config`] with serde-only rates and quotas mixed in.
fn serde_config() -> impl Strategy<Value = LimiterConfig> {
    let extras = proptest::collection::vec(
        (
            proptest::option::of(serde_rate()),
            proptest::option::of(serde_rate()),
            proptest::option::of(serde_rate()),
            proptest::option::of(quota()),
        ),
        4,
    );
    (config(), extras).prop_map(|(mut cfg, extras)| {
        for (l, (a, b, c, q)) in cfg.listeners.iter_mut().zip(extras) {
            l.per_ip_conn_rate = a.or(l.per_ip_conn_rate);
            l.per_ip_bandwidth = b.or(l.per_ip_bandwidth);
            l.bandwidth = c.or(l.bandwidth);
            l.per_ip_conn_quota = q;
        }
        cfg
    })
}

proptest! {
    #[test]
    fn serde_only_settings_round_trip_through_toml(cfg in serde_config()) {
        let text = toml::to_string(&cfg).unwrap();
        prop_assert_eq!(from_toml(&text).unwrap(), cfg);
    }

    #[test]
    fn pacerd_text_round_trips(cfg in config()) {
        let text = cfg.to_pacerd().unwrap();
        prop_assert_eq!(parse_pacerd(&text).unwrap(), cfg);
    }

    #[test]
    fn toml_round_trips(cfg in config()) {
        let text = toml::to_string(&cfg).unwrap();
        prop_assert_eq!(from_toml(&text).unwrap(), cfg);
    }

    /// Whatever the bytes, the reader answers without panicking, and an
    /// accepted file writes back to text that reads the same.
    #[test]
    fn any_bytes_parse_or_fail_cleanly(lines in proptest::collection::vec(
        prop_oneof![
            Just(b"listener a {".to_vec()),
            Just(b"}".to_vec()),
            Just(b"listen 127.0.0.1:1".to_vec()),
            Just(b"upstream [::1]:2".to_vec()),
            "[a-z_]{1,16} [0-9a-z/.:\\[\\]]{0,12}( burst [0-9]{1,3}[kmg]?)?".prop_map(String::into_bytes),
            proptest::collection::vec(any::<u8>(), 0..40),
        ],
        0..20,
    )) {
        let text = lines.join(&b'\n');
        match parse_pacerd(&text) {
            Ok(cfg) => {
                if let Ok(back) = cfg.to_pacerd() {
                    prop_assert_eq!(parse_pacerd(&back).unwrap(), cfg);
                }
            }
            Err(e) => {
                // A generated line can hold newlines of its own, so the
                // bound is the lines of the text, not the pieces joined.
                prop_assert!(e.line <= text.split(|&c| c == b'\n').count());
                prop_assert!(e.message.bytes().all(|c| (0x20..0x7f).contains(&c)), "{}", e);
            }
        }
    }
}
